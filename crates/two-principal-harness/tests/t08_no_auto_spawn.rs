//! T8(a) — a trusted-mode client never auto-spawns a daemon (ADR-0019 §3
//! T8, PR-2's design doc §7, HORO-1835 PR-2c). `ensure_daemon_connection`
//! returns early once a pinned config resolves, so
//! `spawn_daemon_detached` is unreachable in trusted mode (`client.rs`'s
//! own doc comment on that early return).
//!
//! T8(b) — forced trusted-mode start as the wrong principal — is PR-2a
//! scope and already lives in `x_startup_checks.rs` (design doc §7's
//! table places it there, explicitly outside this file, and that file's
//! own doc comment says so). Not duplicated here.
//!
//! Evidence, not exit codes: `hook user-prompt-submit` never exits
//! non-zero by its own never-block/never-panic/never-nonzero-exit
//! contract (`agent/run.rs`'s module doc comment), so this test proves
//! "did it spawn" by looking for the process directly (`pgrep -u
//! <lg-agent-uid>`), not by the hook's exit status.

use std::time::{Duration, Instant};

use libra_governor_two_principal_harness::{
    assert_disposable, query_live, reset_state_dirs, run_as_with_stdin,
    trusted_positive_control_via_cli, ConfigSpec, TrustedConfigGuard, TrustedDaemon, LG_AGENT,
    PROVIDER_BINARY, PROVIDER_STATE_DIR, TRUSTED_CONFIG_PATH,
};

const PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// Comfortably past `client.rs`'s own `SPAWN_WAIT_BUDGET` (3s), plus
/// margin — the design doc's table entry for this test names exactly
/// this bound ("no `lg-agent`-owned daemon process after
/// `SPAWN_WAIT_BUDGET` plus margin").
const SPAWN_WAIT_BUDGET_PLUS_MARGIN: Duration = Duration::from_secs(6);

/// lg-agent's uid (`LG_AGENT.uid`), spelled as a literal because `pgrep
/// -u` takes a string argument and this file has no dependency that
/// would make formatting the constant notably clearer.
const LG_AGENT_UID: &str = "10002";

fn agent_daemon_pids() -> Vec<u32> {
    let out = std::process::Command::new("pgrep")
        .args(["-u", LG_AGENT_UID, "-f", "daemon run"])
        .output()
        .expect("pgrep must be available in the harness image (apt: procps)");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// Kills and reaps any lg-agent-owned `daemon run` process, waiting
/// briefly for the kernel to actually remove it from the process table.
/// `ensure_daemon_connection` spawns detached (`Command::spawn` with no
/// `Child` kept), so there is no handle to wait on directly — this test
/// owns cleanup by uid+cmdline match instead.
fn kill_agent_daemons() {
    for pid in agent_daemon_pids() {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while !agent_daemon_pids().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t08a_trusted_hook_never_spawns_a_daemon() {
    assert_disposable();
    reset_state_dirs();
    kill_agent_daemons();
    let _ = std::fs::remove_dir_all("/home/lg-agent/.local/state/libra-governor");
    assert!(
        agent_daemon_pids().is_empty(),
        "setup: no lg-agent-owned daemon may already be running before this test starts"
    );

    // --- Sensitivity control: config absent. ---
    let preflight_payload = serde_json::json!({
        "session_id": "t08a-sensitivity",
        "cwd": "/home/lg-agent",
        "prompt": "t08a sensitivity control preflight",
    })
    .to_string();
    let out = run_as_with_stdin(
        &LG_AGENT,
        PROVIDER_BINARY,
        &["hook", "user-prompt-submit"],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
        &preflight_payload,
    );
    assert!(
        out.status.success(),
        "the hook must never exit non-zero (default-mode contract)"
    );

    let deadline = Instant::now() + SPAWN_WAIT_BUDGET_PLUS_MARGIN;
    loop {
        if !agent_daemon_pids().is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sensitivity control: with no pinned trusted config, lg-agent's own hook must spawn \
             an lg-agent-owned daemon within the spawn wait budget"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Fully reap the sensitivity control's spawn before the
    // two-principal pass — a leftover process here would make "no
    // lg-agent daemon exists" vacuously true for the wrong reason.
    kill_agent_daemons();
    let _ = std::fs::remove_dir_all("/home/lg-agent/.local/state/libra-governor");
    assert!(
        agent_daemon_pids().is_empty(),
        "setup: the sensitivity control's spawned daemon must be fully reaped before the \
         two-principal pass"
    );

    // --- Two-principal run: config present, no provider running. ---
    reset_state_dirs();
    let spec = ConfigSpec::default();
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);

    let preflight_payload2 = serde_json::json!({
        "session_id": "t08a-two-principal",
        "cwd": "/home/lg-agent",
        "prompt": "t08a two-principal preflight",
    })
    .to_string();
    let out = run_as_with_stdin(
        &LG_AGENT,
        PROVIDER_BINARY,
        &["hook", "user-prompt-submit"],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
        &preflight_payload2,
    );
    assert!(
        out.status.success(),
        "the hook must never exit non-zero even when it reports unavailable"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("daemon unreachable"),
        "the hook must report the daemon as unreachable (no provider was started), not silently \
         succeed via some other path: {stdout}"
    );

    // Give any would-be spawn the full budget plus margin to appear,
    // then assert it never did.
    std::thread::sleep(SPAWN_WAIT_BUDGET_PLUS_MARGIN);
    assert!(
        agent_daemon_pids().is_empty(),
        "trusted mode must never let the hook spawn an lg-agent-owned daemon, even with no \
         provider running to connect to"
    );

    let _ = std::fs::remove_dir_all("/home/lg-agent/.local/state/libra-governor");
}

/// Design doc §7: "In 2c, it switches to the real `hook
/// user-prompt-submit` and `outcome record`." This is the direct proof
/// that [`trusted_positive_control_via_cli`] (this PR's CLI-based
/// replacement for PR-2a/2b's raw-client `trusted_positive_control`, see
/// that function's own doc comment for why it is additive rather than a
/// modification of the PR-2a-owned original) really does drive both
/// halves through the real CLI subcommands and lands one real,
/// non-authoritative `outcome_attestations` row.
#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn trusted_positive_control_via_cli_lands_a_real_non_authoritative_record() {
    assert_disposable();
    reset_state_dirs();
    let spec = ConfigSpec::default();
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);
    let mut daemon = TrustedDaemon::start(TRUSTED_CONFIG_PATH, &[], Duration::from_secs(10));

    let task_id = trusted_positive_control_via_cli(&daemon);
    assert!(
        !task_id.is_empty(),
        "a real task_id must have been assigned"
    );

    let rows = query_live(
        PROVIDER_STATE_DIR,
        "SELECT authoritative FROM outcome_attestations;",
    );
    assert_eq!(
        rows.len(),
        1,
        "exactly one outcome_attestations row must exist after the CLI positive control: {rows:?}"
    );
    assert_eq!(
        rows[0], "0",
        "the CLI positive control's record must be non-authoritative (authority is PR-3 scope)"
    );

    daemon.kill9_and_reap();
}
