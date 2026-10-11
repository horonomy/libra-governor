//! T11(a) — the pinned socket path is never served by a non-provider
//! peer, across many concurrent restart races (ADR-0019 §3 T11, PR-2's
//! design doc §7, PR-2b scope). Companion to `t10c` (a single down
//! window, examined sequentially): this proves the same boundary holds
//! under real concurrent pressure across 50 root-driven `kill -9`/
//! restart cycles, not just once and not just sequentially.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libra_governor_two_principal_harness::{
    assert_disposable, command_as, reset_state_dirs, run_as, ConfigSpec, DaemonProcess, Principal,
    TrustedConfigGuard, TrustedDaemon, LG_AGENT, LG_PROVIDER, PROVIDER_SOCKET, SAMEUID_STATE_DIR,
    TRUSTED_CONFIG_PATH,
};

const PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const RESTART_CYCLES: u32 = 50;

fn fake_server_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lg-fake-server")
}

fn raw_client_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lg-raw-client")
}

/// Sets `stop` on drop, including during an unwind from a failed
/// assertion. A spinning background thread only stops its OWN loop when
/// it next observes `stop`; this is what guarantees that happens even if
/// this test panics mid-run, rather than leaving a detached thread
/// hammering [`PROVIDER_SOCKET`] for the rest of this process's test
/// run (`--test-threads=1`, so every later test shares this process).
struct StopGuard(Arc<AtomicBool>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// `lg-raw-client --print-peer-uid` against `socket`, as `LG_AGENT`.
/// `Some(uid)` iff the connect itself succeeded; `None` on any connect
/// failure (the expected outcome while the provider is down).
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

/// Spins one-shot bind attempts at `socket_path` as `principal` until
/// `stop` is set, counting how many DEMONSTRABLY succeeded (the fake
/// server wrote its ready marker before `stop` was observed). Each
/// attempt uses a fresh, uniquely named count-file so one attempt's
/// success can never be mistaken for another's, and a winning bind is
/// killed immediately rather than left to hold the path indefinitely.
fn spin_bind_attempts(
    principal: Principal,
    socket_path: String,
    stop: Arc<AtomicBool>,
    wins: Arc<AtomicU32>,
) {
    let mut n: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        n += 1;
        let count_file = format!("/tmp/t11a-fake-count-{n}-{}", std::process::id());
        let _ = std::fs::remove_file(&count_file);

        let mut child = match command_as(
            &principal,
            fake_server_bin(),
            &[&socket_path, &count_file],
            &[("PATH", PATH_ENV)],
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        {
            Ok(child) => child,
            Err(_) => continue,
        };

        let deadline = Instant::now() + Duration::from_millis(40);
        let bound = loop {
            if std::path::Path::new(&count_file).exists() {
                break true;
            }
            if let Ok(Some(_)) = child.try_wait() {
                break false;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        if bound {
            wins.fetch_add(1, Ordering::Relaxed);
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&count_file);
    }
}

/// Spins `lg-raw-client --print-peer-uid` probes at `socket_path` until
/// `stop` is set, recording every successfully connected peer uid into
/// `peer_uids`. A failed connect (the common case while the provider is
/// down) is silently skipped. This is PRESSURE, not the non-vacuity
/// proof — the two-principal run's main thread additionally takes one
/// synchronous probe per restart cycle for that (see below), since this
/// background spinner's timing against a deliberately brief 20ms down
/// window is not itself a reliable source of at-least-one-success.
fn spin_probe_attempts(
    socket_path: String,
    stop: Arc<AtomicBool>,
    peer_uids: Arc<Mutex<Vec<u32>>>,
) {
    while !stop.load(Ordering::Relaxed) {
        if let Some(uid) = probe_peer_uid(&socket_path) {
            peer_uids.lock().unwrap().push(uid);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t11a_pinned_socket_is_never_served_by_a_non_provider_peer() {
    assert_disposable();
    reset_state_dirs();

    // --- Sensitivity control: in the same-UID layout, with nobody
    // enforcing any boundary against lg-agent on this path, its own
    // spinning fake server really can win the bind race against a REAL
    // competing restart loop — not merely bind into an empty directory,
    // which would only prove `bind()` works, not that the race itself is
    // winnable. Mirrors the two-principal loop below exactly, but with a
    // same-UID default-mode daemon standing in for the provider. Proves
    // the race mechanism is real in this environment and this container
    // does not simply block all binding races generally — the
    // two-principal run must defeat it through the permission boundary
    // specifically, not through some artifact that would have prevented
    // a win either way. ---
    {
        let socket_path = format!("{SAMEUID_STATE_DIR}/daemon.sock");
        let stop = Arc::new(AtomicBool::new(false));
        let wins = Arc::new(AtomicU32::new(0));
        let _stop_guard = StopGuard(stop.clone());
        let handle = {
            let socket_path = socket_path.clone();
            let stop = stop.clone();
            let wins = wins.clone();
            std::thread::spawn(move || spin_bind_attempts(LG_AGENT, socket_path, stop, wins))
        };

        let mut mirror = DaemonProcess::start(
            &LG_AGENT,
            SAMEUID_STATE_DIR,
            "/home/lg-agent",
            Duration::from_secs(10),
        );
        let mut won = false;
        for _ in 0..RESTART_CYCLES {
            mirror.kill9_and_reap();
            std::thread::sleep(Duration::from_millis(20));
            if wins.load(Ordering::Relaxed) > 0 {
                won = true;
                break;
            }
            mirror = DaemonProcess::start(
                &LG_AGENT,
                SAMEUID_STATE_DIR,
                "/home/lg-agent",
                Duration::from_secs(10),
            );
        }

        stop.store(true, Ordering::Relaxed);
        handle
            .join()
            .expect("sensitivity control bind thread must not panic");
        assert!(
            won || wins.load(Ordering::Relaxed) > 0,
            "sensitivity control: the agent must be able to win the bind race at least once \
             across restarts against its own, unguarded mirror path"
        );
        drop(mirror);
        let _ = std::fs::remove_file(&socket_path);
    }

    // --- Two-principal run: 50 root-driven kill/restart cycles of the
    // REAL trusted provider, while the agent continuously spins bind
    // attempts AND connect probes against the pinned path throughout. ---
    reset_state_dirs();
    let spec = ConfigSpec::default();
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);
    let mut daemon = TrustedDaemon::start(TRUSTED_CONFIG_PATH, &[], Duration::from_secs(10));

    let stop = Arc::new(AtomicBool::new(false));
    let wins = Arc::new(AtomicU32::new(0));
    let peer_uids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let _stop_guard = StopGuard(stop.clone());

    let bind_handle = {
        let stop = stop.clone();
        let wins = wins.clone();
        std::thread::spawn(move || {
            spin_bind_attempts(LG_AGENT, PROVIDER_SOCKET.to_string(), stop, wins)
        })
    };
    let probe_handle = {
        let stop = stop.clone();
        let peer_uids = peer_uids.clone();
        std::thread::spawn(move || {
            spin_probe_attempts(PROVIDER_SOCKET.to_string(), stop, peer_uids)
        })
    };

    for cycle in 0..RESTART_CYCLES {
        assert!(
            daemon.is_alive(),
            "cycle {cycle}: the provider must be alive before this restart"
        );
        daemon.kill9_and_reap();
        // A brief, deliberately short down window — long enough for the
        // spinning threads to get several attempts in per cycle, short
        // enough to keep all 50 cycles fast.
        std::thread::sleep(Duration::from_millis(20));
        daemon = TrustedDaemon::start(TRUSTED_CONFIG_PATH, &[], Duration::from_secs(10));

        // A synchronous probe, from the main thread, once per cycle —
        // the actual non-vacuity guarantee. The background spinner above
        // is pressure; its timing against this deliberately brief down
        // window is not itself a reliable source of at-least-one
        // success, so this is what guarantees RESTART_CYCLES real
        // successful connects land in `peer_uids` regardless of
        // scheduling luck.
        let uid = probe_peer_uid(PROVIDER_SOCKET);
        assert_eq!(
            uid,
            Some(LG_PROVIDER.uid),
            "cycle {cycle}: a probe against the just-restarted provider must succeed and \
             report its peer uid"
        );
        peer_uids.lock().unwrap().push(uid.unwrap());
    }

    stop.store(true, Ordering::Relaxed);
    bind_handle
        .join()
        .expect("bind-attempt thread must not panic");
    probe_handle.join().expect("probe thread must not panic");

    assert_eq!(
        wins.load(Ordering::Relaxed),
        0,
        "the agent must never win the bind race at the pinned socket path across \
         {RESTART_CYCLES} restarts"
    );

    let uids = peer_uids.lock().unwrap();
    assert!(
        !uids.is_empty(),
        "at least one connect probe must have succeeded across {RESTART_CYCLES} restarts, \
         otherwise the peer-uid assertion below would be vacuous"
    );
    assert!(
        uids.iter().all(|&uid| uid == LG_PROVIDER.uid),
        "every successful probe must report the provider's peer uid, never any other \
         (observed uids: {uids:?})"
    );

    daemon.kill9_and_reap();
}
