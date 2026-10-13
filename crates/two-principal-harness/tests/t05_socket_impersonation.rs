//! T5 — socket impersonation (ADR-0019 §3 T5, PR-2's design doc §7,
//! HORO-1835 PR-2c).
//!
//! # Deviations from the design doc (documented, not silent)
//!
//! **No `lg-fake-server`.** The design doc's §7 table assigns T5's
//! `bind()`-attempt sub-case and the general "fake server" role in T7(b)
//! /T10(c)/T11(a) to a helper binary, `lg-fake-server`, which §9 places
//! in PR-2b ("server race and restart evidence... plus
//! `lg-fake-server`"), explicitly independent of this PR. Building it
//! here would be PR-2b scope creep. `t05a` therefore proves the socket
//! directory is unwritable using only directory-mutation primitives
//! (`touch`/`mv`/`rm`/`ln -s`) — a `bind()` needs exactly the same
//! directory-create permission a `touch` does, so the directory-level
//! proof already covers it. `t05b` and `t05c` substitute a real
//! [`DaemonProcess`] — today's already-built, unmodified binary running
//! as `lg-agent` — for the design doc's "fake server", asserting on real
//! ledger row counts (`query_live`) rather than a byte-count file
//! `lg-fake-server` would have written. This is a stronger proof, not a
//! weaker one: the "fake" the agent redirects a client toward is a real,
//! fully functioning daemon, not a byte sink.
//!
//! **`t05c`'s "same config, differs only in `provider_uid`" shape.**
//! Per the design doc's own note on this exact case ("otherwise
//! `validate_fields` rejects it before connect"), the sensitivity
//! config's `provider_uid` must equal the fake server's real uid
//! (`lg-agent`'s, `10002`) for the config to validate at all, which in
//! turn forces `agent_uids` to name some other uid — `lg-provider`'s is
//! used here, since it is already a real, meaningful uid in this
//! harness, even though nothing in this test exercises it as an agent
//! peer.

use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use libra_governor_two_principal_harness::{
    assert_disposable, query_live, reset_state_dirs, run_as, run_as_with_stdin, ConfigSpec,
    DaemonProcess, TrustedConfigGuard, TrustedDaemon, LG_AGENT, LG_PROVIDER, PROVIDER_BINARY,
    PROVIDER_SOCKET, PROVIDER_SOCKET_DIR, PROVIDER_STATE_DIR, SAMEUID_STATE_DIR,
    TRUSTED_CONFIG_PATH,
};

const PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn denied(out: &std::process::Output) -> bool {
    !out.status.success() && {
        let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
        stderr.contains("permission denied") || stderr.contains("not permitted")
    }
}

fn socket_inode(path: &str) -> u64 {
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {path}: {e}"))
        .ino()
}

fn task_count(state_dir: &str) -> i64 {
    query_live(state_dir, "SELECT COUNT(*) FROM tasks;")
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn outcome_attestation_count(state_dir: &str) -> i64 {
    query_live(state_dir, "SELECT COUNT(*) FROM outcome_attestations;")
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t05a_agent_cannot_create_rename_or_unlink_in_the_socket_dir() {
    assert_disposable();
    reset_state_dirs();

    // --- Sensitivity control: the SAME directory-write operations
    // against the agent's own mirror layout must succeed. ---
    let touch = run_as(
        &LG_AGENT,
        "sh",
        &["-c", &format!("touch {SAMEUID_STATE_DIR}/evil.sock")],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        touch.status.success(),
        "sensitivity control: lg-agent must be able to create a file in its own mirror dir: {}",
        String::from_utf8_lossy(&touch.stderr)
    );
    let mv = run_as(
        &LG_AGENT,
        "mv",
        &[
            &format!("{SAMEUID_STATE_DIR}/evil.sock"),
            &format!("{SAMEUID_STATE_DIR}/evil2.sock"),
        ],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        mv.status.success(),
        "sensitivity control: mv must succeed: {}",
        String::from_utf8_lossy(&mv.stderr)
    );
    let ln = run_as(
        &LG_AGENT,
        "ln",
        &[
            "-s",
            "/dev/null",
            &format!("{SAMEUID_STATE_DIR}/evil3.sock"),
        ],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        ln.status.success(),
        "sensitivity control: ln -s must succeed: {}",
        String::from_utf8_lossy(&ln.stderr)
    );
    let rm = run_as(
        &LG_AGENT,
        "rm",
        &[
            "-f",
            &format!("{SAMEUID_STATE_DIR}/evil2.sock"),
            &format!("{SAMEUID_STATE_DIR}/evil3.sock"),
        ],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        rm.status.success(),
        "sensitivity control: rm must succeed: {}",
        String::from_utf8_lossy(&rm.stderr)
    );

    // --- Two-principal run: a real trusted daemon is listening, so
    // there is a real socket inode to prove unchanged. ---
    reset_state_dirs();
    let spec = ConfigSpec::default();
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);
    let mut daemon = TrustedDaemon::start(TRUSTED_CONFIG_PATH, &[], Duration::from_secs(10));
    let before_inode = socket_inode(PROVIDER_SOCKET);

    let touch = run_as(
        &LG_AGENT,
        "sh",
        &["-c", &format!("touch {PROVIDER_SOCKET_DIR}/evil.sock")],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        denied(&touch),
        "lg-agent must not be able to create a file in the socket dir; got status={:?} stderr={}",
        touch.status,
        String::from_utf8_lossy(&touch.stderr)
    );

    let mv = run_as(
        &LG_AGENT,
        "mv",
        &[
            PROVIDER_SOCKET,
            &format!("{PROVIDER_SOCKET_DIR}/evil2.sock"),
        ],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        denied(&mv),
        "lg-agent must not be able to mv the provider's socket; got status={:?} stderr={}",
        mv.status,
        String::from_utf8_lossy(&mv.stderr)
    );

    let rm = run_as(
        &LG_AGENT,
        "rm",
        &["-f", PROVIDER_SOCKET],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        denied(&rm),
        "lg-agent must not be able to rm the provider's socket; got status={:?} stderr={}",
        rm.status,
        String::from_utf8_lossy(&rm.stderr)
    );

    let ln = run_as(
        &LG_AGENT,
        "ln",
        &[
            "-s",
            "/dev/null",
            &format!("{PROVIDER_SOCKET_DIR}/evil3.sock"),
        ],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        denied(&ln),
        "lg-agent must not be able to symlink into the socket dir; got status={:?} stderr={}",
        ln.status,
        String::from_utf8_lossy(&ln.stderr)
    );

    let after_inode = socket_inode(PROVIDER_SOCKET);
    assert_eq!(
        before_inode, after_inode,
        "the provider's socket inode must be unchanged after every failed attack"
    );
    assert!(daemon.is_alive(), "the provider must still be alive");

    daemon.kill9_and_reap();
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t05b_env_pollution_never_redirects_a_trusted_client() {
    assert_disposable();
    reset_state_dirs();

    // --- Sensitivity control: config absent — the hook follows
    // LIBRA_GOVERNOR_STATE_DIR exactly as default mode always has. ---
    let mut fake = DaemonProcess::start(
        &LG_AGENT,
        SAMEUID_STATE_DIR,
        "/home/lg-agent",
        Duration::from_secs(10),
    );
    let before_count = task_count(SAMEUID_STATE_DIR);

    let preflight_payload = serde_json::json!({
        "session_id": "t05b-sensitivity",
        "cwd": SAMEUID_STATE_DIR,
        "prompt": "t05b sensitivity control preflight",
    })
    .to_string();
    let out = run_as_with_stdin(
        &LG_AGENT,
        PROVIDER_BINARY,
        &["hook", "user-prompt-submit"],
        &[
            ("PATH", PATH_ENV),
            ("HOME", "/home/lg-agent"),
            ("LIBRA_GOVERNOR_STATE_DIR", SAMEUID_STATE_DIR),
        ],
        &preflight_payload,
    );
    assert!(
        out.status.success(),
        "the hook must never exit non-zero (default-mode contract)"
    );
    let after_count = task_count(SAMEUID_STATE_DIR);
    assert!(
        after_count > before_count,
        "sensitivity control: with no pinned config, the hook must follow \
         LIBRA_GOVERNOR_STATE_DIR to the fake daemon and create a task there"
    );

    fake.kill9_and_reap();

    // --- Two-principal run: config present. The SAME env override is
    // set, but the pinned config must win. ---
    reset_state_dirs();
    let mut fake2 = DaemonProcess::start(
        &LG_AGENT,
        SAMEUID_STATE_DIR,
        "/home/lg-agent",
        Duration::from_secs(10),
    );
    let fake_before = task_count(SAMEUID_STATE_DIR);

    let spec = ConfigSpec::default();
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);
    let mut real_daemon = TrustedDaemon::start(TRUSTED_CONFIG_PATH, &[], Duration::from_secs(10));
    let real_before = task_count(PROVIDER_STATE_DIR);

    let preflight_payload2 = serde_json::json!({
        "session_id": "t05b-two-principal",
        "cwd": SAMEUID_STATE_DIR,
        "prompt": "t05b two-principal preflight",
    })
    .to_string();
    let out = run_as_with_stdin(
        &LG_AGENT,
        PROVIDER_BINARY,
        &["hook", "user-prompt-submit"],
        &[
            ("PATH", PATH_ENV),
            ("HOME", "/home/lg-agent"),
            ("LIBRA_GOVERNOR_STATE_DIR", SAMEUID_STATE_DIR),
        ],
        &preflight_payload2,
    );
    assert!(out.status.success(), "the hook must never exit non-zero");

    let fake_after = task_count(SAMEUID_STATE_DIR);
    assert_eq!(
        fake_before, fake_after,
        "the env-pointed fake daemon must receive nothing once a pinned trusted config exists"
    );
    let real_after = task_count(PROVIDER_STATE_DIR);
    assert!(
        real_after > real_before,
        "the hook must have reached the real provider instead of the env-named fake one"
    );
    assert!(
        real_daemon.is_alive(),
        "the real provider must still be alive"
    );

    real_daemon.kill9_and_reap();
    fake2.kill9_and_reap();
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t05c_trusted_client_refuses_a_server_with_the_wrong_peer_uid() {
    assert_disposable();
    reset_state_dirs();

    // The "fake server" is a real, already-built default-mode daemon
    // running as lg-agent, proving connectivity against a real process
    // rather than a stub.
    let mut fake = DaemonProcess::start(
        &LG_AGENT,
        SAMEUID_STATE_DIR,
        "/home/lg-agent",
        Duration::from_secs(10),
    );

    // Seed a real task directly against it (today's default-mode hook,
    // no pinned config involved yet) so `outcome record` below has a
    // real `task_id` to record against.
    let seed_payload = serde_json::json!({
        "session_id": "t05c-seed",
        "cwd": SAMEUID_STATE_DIR,
        "prompt": "t05c seed task",
    })
    .to_string();
    let seed = run_as_with_stdin(
        &LG_AGENT,
        PROVIDER_BINARY,
        &["hook", "user-prompt-submit"],
        &[
            ("PATH", PATH_ENV),
            ("HOME", "/home/lg-agent"),
            // No pinned config exists yet at this point in the test, so
            // this is an ordinary default-mode env override pointing the
            // hook at the fake server's own state dir — exactly like
            // `t05b`'s sensitivity control. Without this, the hook would
            // resolve its default `$HOME/.local/state/libra-governor`
            // instead, find no daemon listening there, and create no
            // task anywhere this test can see.
            ("LIBRA_GOVERNOR_STATE_DIR", SAMEUID_STATE_DIR),
        ],
        &seed_payload,
    );
    assert!(
        seed.status.success(),
        "setup: seeding a task against the fake server must succeed"
    );
    let task_id = query_live(
        SAMEUID_STATE_DIR,
        "SELECT task_id FROM tasks ORDER BY created_at DESC LIMIT 1;",
    )
    .into_iter()
    .next()
    .expect("setup: a seed task must exist in the fake server's ledger");

    // --- Sensitivity control: the config correctly names the fake
    // server's REAL uid as provider_uid, so the identity check passes
    // and the request lands. ---
    let sensitivity_spec = ConfigSpec {
        provider_uid: LG_AGENT.uid,
        agent_uids: vec![LG_PROVIDER.uid],
        socket_dir: SAMEUID_STATE_DIR.to_string(),
        ..ConfigSpec::default()
    };
    {
        let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &sensitivity_spec);
        let outcome_payload = serde_json::json!({
            "task_id": task_id,
            "source_id": "t05c-sensitivity",
            "idempotency_key": "t05c-sensitivity-1",
            "outcome": {"kind": "completed", "evidence": ["harness://t05c-sensitivity"]},
        })
        .to_string();
        let out = run_as_with_stdin(
            &LG_AGENT,
            PROVIDER_BINARY,
            &["outcome", "record"],
            &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
            &outcome_payload,
        );
        assert!(
            out.status.success(),
            "sensitivity control: outcome record must succeed when the configured \
             provider_uid matches the fake server's real uid: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("\"state\":\"recorded\""),
            "expected a Recorded response, got: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    // --- Two-principal run: the config names the REAL provider uid,
    // but socket_dir still points at the agent's fake server. The
    // identity check must now refuse, before a byte is sent. ---
    let mismatch_spec = ConfigSpec {
        socket_dir: SAMEUID_STATE_DIR.to_string(),
        ..ConfigSpec::default()
    };
    let before = outcome_attestation_count(SAMEUID_STATE_DIR);
    {
        let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &mismatch_spec);
        let outcome_payload = serde_json::json!({
            "task_id": task_id,
            "source_id": "t05c-two-principal",
            "idempotency_key": "t05c-two-principal-1",
            "outcome": {"kind": "completed", "evidence": ["harness://t05c-two-principal"]},
        })
        .to_string();
        let out = run_as_with_stdin(
            &LG_AGENT,
            PROVIDER_BINARY,
            &["outcome", "record"],
            &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
            &outcome_payload,
        );
        assert!(
            !out.status.success(),
            "the client must refuse a server whose peer uid does not match the pinned config"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(
                "the server's identity did not match the pinned trusted-principal configuration"
            ),
            "expected the identity-mismatch error text, got: {stderr}"
        );
    }
    let after = outcome_attestation_count(SAMEUID_STATE_DIR);
    assert_eq!(
        before, after,
        "the fake server must receive nothing once the peer-identity check refuses"
    );

    fake.kill9_and_reap();
}
