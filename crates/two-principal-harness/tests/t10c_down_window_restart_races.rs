//! T10(c) — the down window after a `kill -9` and clean restart recovery
//! (ADR-0019 §3 T10, PR-2's design doc §7, PR-2b scope). Companion to
//! `t10b` (no client-triggered shutdown): this proves the complementary
//! window — once the provider is killed outright (leaving a stale
//! socket file), nothing the agent can do from its own UID reaches that
//! path while it is down, and a root-driven restart recovers cleanly,
//! unlinking the stale file and rebinding as the provider.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use libra_governor_two_principal_harness::{
    assert_disposable, command_as, reset_state_dirs, run_as, ConfigSpec, Principal,
    TrustedConfigGuard, TrustedDaemon, LG_AGENT, LG_PROVIDER, PROVIDER_SOCKET, PROVIDER_SOCKET_DIR,
    PROVIDER_STATE_DIR, SAMEUID_STATE_DIR, TRUSTED_CONFIG_PATH,
};

const PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn fake_server_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lg-fake-server")
}

fn raw_client_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lg-raw-client")
}

fn denied(out: &Output) -> bool {
    !out.status.success() && {
        let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
        stderr.contains("permission denied") || stderr.contains("not permitted")
    }
}

/// `lg-raw-client --print-peer-uid` against `socket`, as `LG_AGENT`.
/// `Some(uid)` iff the connect itself succeeded (whoever is actually
/// listening there right now); `None` on any connect failure (the
/// expected outcome against a down or stale-only socket).
fn probe_peer_uid(socket: &str) -> Option<u32> {
    let out = run_as(
        &LG_AGENT,
        raw_client_bin(),
        &[socket, "{}", "--print-peer-uid"],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("peer_uid="))
        .and_then(|uid| uid.trim().parse().ok())
}

/// A single bind attempt at `socket_path`, as `principal`, via
/// `lg-fake-server`. Detected via the ready-signal `count-file`
/// `lg-fake-server` writes right after a successful `bind` — never by
/// waiting on the child directly, since a winning bind never returns on
/// its own. Returns `true` iff the bind demonstrably succeeded, and
/// always kills the (now unnecessary either way) child before returning.
fn attempt_bind(principal: &Principal, socket_path: &str, tag: &str) -> bool {
    let count_file = format!(
        "/tmp/t10c-fake-count-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _ = std::fs::remove_file(&count_file);

    let mut child = command_as(
        principal,
        fake_server_bin(),
        &[socket_path, &count_file],
        &[("PATH", PATH_ENV)],
    )
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap_or_else(|e| panic!("failed to spawn lg-fake-server: {e}"));

    let deadline = Instant::now() + Duration::from_millis(300);
    let bound = loop {
        if Path::new(&count_file).exists() {
            break true;
        }
        if let Ok(Some(_)) = child.try_wait() {
            break false;
        }
        if Instant::now() >= deadline {
            // Still running but never wrote the ready marker within the
            // deadline: treat conservatively as "did not demonstrably
            // bind" rather than hanging this helper forever.
            break false;
        }
        std::thread::sleep(Duration::from_millis(5));
    };

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&count_file);
    bound
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t10c_down_window_create_bind_connect_all_fail() {
    assert_disposable();
    reset_state_dirs();

    // --- Sensitivity control: the SAME bind primitive, against the
    // agent's own unguarded mirror path, really can succeed. Proves the
    // mechanism itself is real in this environment, not something this
    // container blocks generally. ---
    {
        let socket_path = format!("{SAMEUID_STATE_DIR}/daemon.sock");
        assert!(
            attempt_bind(&LG_AGENT, &socket_path, "sensitivity"),
            "sensitivity control: lg-agent must be able to bind a fake server in its own, \
             entirely self-owned mirror directory"
        );
        let _ = std::fs::remove_file(&socket_path);
    }

    // --- Two-principal run. ---
    reset_state_dirs();
    let spec = ConfigSpec::default();
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);
    let mut daemon = TrustedDaemon::start(TRUSTED_CONFIG_PATH, &[], Duration::from_secs(10));
    assert!(
        daemon.is_alive(),
        "the trusted provider must start up alive"
    );
    // Prove the socket is actually served by the real provider BEFORE
    // the kill — otherwise a later "the restart served it" assertion
    // would not distinguish a real restart from "it was always like
    // this" (e.g. if the kill never actually landed).
    assert_eq!(
        probe_peer_uid(PROVIDER_SOCKET),
        Some(LG_PROVIDER.uid),
        "the provider must be the one actually serving the socket before it is killed"
    );

    daemon.kill9_and_reap();
    assert!(
        Path::new(PROVIDER_SOCKET).exists(),
        "a kill -9 must leave the stale socket file behind, not clean it up"
    );

    // Down window: for a short span, the agent repeatedly attempts to
    // create, bind, and unlink at the pinned path, in BOTH the state dir
    // and the socket dir. Every attempt must fail, and a connect attempt
    // must get ECONNREFUSED against the stale, unserved file.
    let deadline = Instant::now() + Duration::from_millis(800);
    let mut attempts: u32 = 0;
    while Instant::now() < deadline && attempts < 20 {
        attempts += 1;

        let touch_state = run_as(
            &LG_AGENT,
            "sh",
            &["-c", &format!("touch {PROVIDER_STATE_DIR}/evil.sock")],
            &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
        );
        assert!(
            denied(&touch_state),
            "attempt {attempts}: agent must not be able to create a file in the provider's \
             state dir during the down window; got status={:?} stderr={}",
            touch_state.status,
            String::from_utf8_lossy(&touch_state.stderr)
        );

        let touch_socket = run_as(
            &LG_AGENT,
            "sh",
            &["-c", &format!("touch {PROVIDER_SOCKET_DIR}/evil.sock")],
            &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
        );
        assert!(
            denied(&touch_socket),
            "attempt {attempts}: agent must not be able to create a file in the provider's \
             socket dir during the down window; got status={:?} stderr={}",
            touch_socket.status,
            String::from_utf8_lossy(&touch_socket.stderr)
        );

        let rm = run_as(
            &LG_AGENT,
            "rm",
            &["-f", PROVIDER_SOCKET],
            &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
        );
        assert!(
            denied(&rm),
            "attempt {attempts}: agent must not be able to unlink the stale provider socket \
             during the down window; got status={:?} stderr={}",
            rm.status,
            String::from_utf8_lossy(&rm.stderr)
        );

        assert!(
            !attempt_bind(&LG_AGENT, PROVIDER_SOCKET, &format!("down-{attempts}")),
            "attempt {attempts}: agent must never win the bind race at the pinned socket path \
             during the down window"
        );

        let out = run_as(
            &LG_AGENT,
            raw_client_bin(),
            &[PROVIDER_SOCKET, "{}"],
            &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
        );
        assert!(
            !out.status.success(),
            "attempt {attempts}: connect to the stale, unserved socket must fail"
        );
        let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
        assert!(
            stderr.contains("refused"),
            "attempt {attempts}: expected an ECONNREFUSED-shaped error, got: {stderr}"
        );
    }
    assert!(attempts > 0, "the down-window loop must run at least once");

    // Root restarts the provider.
    let mut restarted = TrustedDaemon::start(TRUSTED_CONFIG_PATH, &[], Duration::from_secs(10));
    assert!(
        restarted.is_alive(),
        "the restarted provider must come up alive"
    );

    // Not an inode comparison: on this container's filesystem, a freed
    // inode number can legitimately be reused for the very next file
    // created, so "the new socket's inode differs from the stale one" is
    // not a reliable signal either way. Ownership plus an actual
    // successful identity-bearing probe is the real proof that this is
    // a fresh bind by the provider, not the stale file somehow still
    // being served.
    let meta = std::fs::metadata(PROVIDER_SOCKET)
        .unwrap_or_else(|e| panic!("stat {PROVIDER_SOCKET}: {e}"));
    assert_eq!(
        meta.uid(),
        LG_PROVIDER.uid,
        "the freshly bound socket must be owned by the provider"
    );
    assert_eq!(
        probe_peer_uid(PROVIDER_SOCKET),
        Some(LG_PROVIDER.uid),
        "the restarted socket must actually be served by the provider"
    );

    // Prove it is not merely alive but actually serving.
    let out = run_as(
        &LG_AGENT,
        raw_client_bin(),
        &[
            PROVIDER_SOCKET,
            r#"{"protocol_version":14,"request":{"kind":"status"}}"#,
        ],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        out.status.success(),
        "the restarted provider must serve a normal request: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    restarted.kill9_and_reap();
}
