//! Trusted mode's startup checks (ADR-0019 §1 "Launch and startup
//! checks", PR-2's design doc §3/§7). Each broken-layout case is
//! preceded by a passing start on the correct layout (the design doc's
//! table note: "Each run is preceded by a passing start on the correct
//! layout"), so a refusal here cannot mean "the whole mechanism is
//! unreachable".
//!
//! Also hosts `t08b` (design doc §7's table places it in this file,
//! explicitly outside `t08_no_auto_spawn.rs`, which is PR-2c scope):
//! forced trusted-mode starts as the wrong principal.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use libra_governor_two_principal_harness::{
    assert_disposable, reset_state_dirs, ConfigSpec, TrustedConfigGuard, TrustedDaemon, LG_AGENT,
    LG_PROVIDER, PROVIDER_SOCKET_DIR, PROVIDER_STATE_DIR, TRUSTED_CONFIG_PATH,
};

/// A short-lived passing start + kill, confirming the baseline layout
/// really does work before the next assertion breaks one part of it.
fn passing_baseline() {
    reset_state_dirs();
    let spec = ConfigSpec::default();
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);
    let mut daemon = TrustedDaemon::start(TRUSTED_CONFIG_PATH, &[], Duration::from_secs(10));
    assert!(daemon.is_alive(), "baseline trusted start must succeed");
    daemon.kill9_and_reap();
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn x_startup_refuses_wrong_uid() {
    assert_disposable();
    passing_baseline();
    reset_state_dirs();

    let spec = ConfigSpec::default(); // provider_uid: LG_PROVIDER.uid
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);
    let output = TrustedDaemon::start_expecting_refusal(
        &LG_AGENT,
        TRUSTED_CONFIG_PATH,
        &[("HOME", "/home/lg-agent")],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("real and effective uid must equal"),
        "expected the process-identity refusal text, got: {stderr}"
    );
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn x_startup_refuses_group_writable_config() {
    assert_disposable();
    passing_baseline();
    reset_state_dirs();

    let spec = ConfigSpec::default();
    // Root-owned, but group-writable (0664) — the config-file check must
    // reject this even though ownership itself is fine.
    let _guard = TrustedConfigGuard::install_with(TRUSTED_CONFIG_PATH, &spec, "0:0", 0o664);
    let output = TrustedDaemon::start_expecting_refusal(
        &LG_PROVIDER,
        TRUSTED_CONFIG_PATH,
        &[("HOME", "/root")],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("config file must be owned by uid 0"),
        "expected the config-file refusal text, got: {stderr}"
    );
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn x_startup_refuses_agent_writable_ancestor() {
    assert_disposable();
    passing_baseline();
    reset_state_dirs();

    // /tmp is world-writable (mode 1777) in this image, same as any real
    // Linux system — using it as the socket dir's PARENT trips the
    // ancestor walk without this test mutating any shared container
    // state (no chmod/chown to undo).
    let evil_socket_dir = "/tmp/x-startup-evil-socket-dir";
    let _ = std::fs::remove_dir_all(evil_socket_dir);
    std::fs::create_dir_all(evil_socket_dir).unwrap();
    std::fs::set_permissions(evil_socket_dir, std::fs::Permissions::from_mode(0o2710)).unwrap();

    let spec = ConfigSpec {
        socket_dir: evil_socket_dir.to_string(),
        ..ConfigSpec::default()
    };
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);
    let output = TrustedDaemon::start_expecting_refusal(
        &LG_PROVIDER,
        TRUSTED_CONFIG_PATH,
        &[("HOME", "/root")],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is not owned by root or the provider"),
        "expected the path-chain refusal text, got: {stderr}"
    );

    let _ = std::fs::remove_dir_all(evil_socket_dir);
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn x_startup_refuses_state_dir_not_0700() {
    assert_disposable();
    passing_baseline();
    reset_state_dirs();
    std::fs::set_permissions(PROVIDER_STATE_DIR, std::fs::Permissions::from_mode(0o750)).unwrap();

    let spec = ConfigSpec::default();
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);
    let output = TrustedDaemon::start_expecting_refusal(
        &LG_PROVIDER,
        TRUSTED_CONFIG_PATH,
        &[("HOME", "/root")],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("state dir must be owned by the provider with mode 0700"),
        "expected the state-dir refusal text, got: {stderr}"
    );
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn x_startup_refuses_socket_dir_other_accessible() {
    assert_disposable();
    passing_baseline();
    reset_state_dirs();
    // Other-readable+executable (mode & 0o007 != 0), which the socket-dir
    // check rejects even though it is not group- or other-WRITABLE.
    std::fs::set_permissions(PROVIDER_SOCKET_DIR, std::fs::Permissions::from_mode(0o2715)).unwrap();

    let spec = ConfigSpec::default();
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);
    let output = TrustedDaemon::start_expecting_refusal(
        &LG_PROVIDER,
        TRUSTED_CONFIG_PATH,
        &[("HOME", "/root")],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("socket dir must be owned by the provider"),
        "expected the socket-dir refusal text, got: {stderr}"
    );
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn x_startup_refuses_missing_append_only_trigger() {
    assert_disposable();
    passing_baseline();
    reset_state_dirs();

    // Start once (as the provider) so the ledger file exists with every
    // migration applied, then kill it before dropping a trigger as the
    // SAME uid that owns the file — running sqlite3 as root here would
    // leave root-owned -wal/-shm sidecars behind that the provider could
    // then no longer open, producing a Ledger I/O refusal instead of the
    // specific MissingTrigger one this test means to prove.
    let spec = ConfigSpec::default();
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);
    {
        let mut daemon = TrustedDaemon::start(TRUSTED_CONFIG_PATH, &[], Duration::from_secs(10));
        daemon.kill9_and_reap();
    }
    assert!(
        Path::new(PROVIDER_STATE_DIR)
            .join("ledger.sqlite3")
            .exists(),
        "ledger must exist after the first successful start"
    );

    let drop_trigger = libra_governor_two_principal_harness::run_as(
        &LG_PROVIDER,
        "sqlite3",
        &[
            &format!("{PROVIDER_STATE_DIR}/ledger.sqlite3"),
            "DROP TRIGGER trg_outcome_attestations_no_delete;",
        ],
        &[
            (
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            ),
            ("HOME", "/root"),
        ],
    );
    assert!(
        drop_trigger.status.success(),
        "setup: root-as-provider DROP TRIGGER must succeed: {}",
        String::from_utf8_lossy(&drop_trigger.stderr)
    );

    let output = TrustedDaemon::start_expecting_refusal(
        &LG_PROVIDER,
        TRUSTED_CONFIG_PATH,
        &[("HOME", "/root")],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("missing required append-only trigger")
            && stderr.contains("trg_outcome_attestations_no_delete"),
        "expected the missing-trigger refusal text naming the dropped trigger, got: {stderr}"
    );
}

/// t08b — forced trusted-mode starts as the wrong principal (ADR-0019 §3
/// T8, design doc §7's table). Two cases: the legitimate start as the
/// provider succeeds (reusing `passing_baseline`'s own proof), and an
/// agent-authored, agent-owned config — naming the AGENT's own uid as
/// `provider_uid`, so the process-identity check alone would pass —
/// is still refused because the config file itself is not root-owned.
#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t08b_forced_trusted_start_as_agent_refuses() {
    assert_disposable();
    passing_baseline(); // the legitimate start, as the provider, works.
    reset_state_dirs();

    let evil_config_path = "/home/lg-agent/evil-trusted-principal.json";
    let evil_spec = ConfigSpec {
        provider_uid: LG_AGENT.uid,
        agent_uids: vec![99_999],
        ..ConfigSpec::default()
    };
    let _guard =
        TrustedConfigGuard::install_with(evil_config_path, &evil_spec, "10002:10002", 0o644);

    let output = TrustedDaemon::start_expecting_refusal(
        &LG_AGENT,
        evil_config_path,
        &[("HOME", "/home/lg-agent")],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("config file must be owned by uid 0"),
        "expected the config-file refusal text (process identity alone is not enough to start \
         trusted mode as the agent), got: {stderr}"
    );
}
