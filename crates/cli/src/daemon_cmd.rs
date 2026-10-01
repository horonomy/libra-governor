//! `libra-governor daemon run` — the foreground daemon process entry
//! point. Spawned detached by [`crate::client::ensure_daemon_connection`]
//! when the `hook` subcommand finds no daemon listening.
//!
//! Also `libra-governor daemon stop` (HORO-1380): an identity-checked
//! replacement for the `pkill -f "libra-governor daemon run"` guidance
//! this binary used to print. `pkill -f` name-matches every Governor
//! daemon on the host across every state dir, including ones this
//! operator does not own — `stop` instead reads the pid record the
//! running daemon wrote for itself
//! ([`libra_governor_daemon::pidfile`]) and signals only that exact
//! process, after verifying it is (a) still alive, (b) still running the
//! same executable the record names, and (c) still answering on the
//! daemon's own socket. Any check failing refuses to signal anything.

use libra_governor_daemon::{recon::ReconBudget, DaemonConfig, DaemonError};
use libra_governor_domain::ReplanHysteresisConfig;
use libra_governor_protocol::{Request, Response};
use std::path::Path;
#[cfg(not(target_os = "linux"))]
use std::path::PathBuf;
use std::process::Command;

/// Serializes tests in this module that mutate `LIBRA_GOVERNOR_STATE_DIR`
/// (a process-global) so they cannot observe each other's env changes.
/// Mirrors the identical pattern in `libra_governor_daemon::paths`'s own
/// tests.
#[cfg(test)]
pub(crate) fn state_dir_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &LOCK
}

/// Default TTL (HORO-1141) for how long a reservation may stay `Active`
/// before startup/opportunistic reconciliation reclaims it — generous
/// enough to cover a normal task's exploration-through-finalization span
/// without being effectively unbounded.
const DEFAULT_RESERVATION_TTL_SECS: u64 = 900;

/// Runs the daemon in the foreground: resolves state paths, binds the
/// socket (recovering a stale socket file, or exiting cleanly if another
/// daemon already owns it — see
/// `libra-governor-daemon::server::bind_or_detect_running`), and serves
/// forever.
pub fn run() {
    let state_dir = match libra_governor_daemon::paths::ensure_state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("libra-governor daemon: could not prepare state dir: {e}");
            std::process::exit(1);
        }
    };

    let log_path = state_dir.join("daemon.log");

    // Optional, additive `config.json` overrides (HORO-1146): selects a
    // non-default Policy preset and/or turns the gateway on. Absent or
    // partially specified is not an error — see
    // `libra_governor_daemon::config_file` module docs — and a present
    // but invalid file falls back to today's hardcoded defaults rather
    // than aborting startup, logged so it is visible rather than silent.
    let (policy, gateway, extensions) =
        match libra_governor_daemon::config_file::load_overrides(&state_dir) {
            Ok((policy, gateway, extensions)) => (policy, gateway, extensions),
            Err(e) => {
                libra_governor_daemon::log::append_line(
                    &log_path,
                    &format!(
                    "daemon: {} rejected — falling back to default policy/gateway/extensions: {e}",
                    libra_governor_daemon::config_file::CONFIG_FILE_NAME
                ),
                );
                (None, None, None)
            }
        };

    let config = DaemonConfig {
        socket_path: state_dir.join("daemon.sock"),
        ledger_path: state_dir.join("ledger.sqlite3"),
        log_path,
        recon_budget: ReconBudget::default(),
        replan_hysteresis: ReplanHysteresisConfig::default(),
        policy: policy.unwrap_or_else(libra_governor_daemon::default_admission_policy),
        reservation_ttl_secs: DEFAULT_RESERVATION_TTL_SECS,
        gateway,
        gateway_stats: std::sync::Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
        extensions,
        extension_runtime: std::sync::OnceLock::new(),
    };

    let listener = match libra_governor_daemon::bind_or_detect_running(&config.socket_path) {
        Ok(listener) => listener,
        Err(DaemonError::AlreadyRunning(path)) => {
            // Not an error from the caller's perspective: whichever
            // daemon spawned first wins, and losing this race is the
            // expected outcome of the spawn-if-absent pattern.
            libra_governor_daemon::log::append_line(
                &config.log_path,
                &format!("daemon already running at {}; exiting", path.display()),
            );
            return;
        }
        Err(e) => {
            eprintln!("libra-governor daemon: could not bind socket: {e}");
            std::process::exit(1);
        }
    };

    // Record this daemon's identity for `daemon stop` (HORO-1380).
    // Best-effort: a failure here does not stop the daemon from serving —
    // it only means `daemon stop` will find no usable record and refuse
    // to signal anything, which is the safe failure mode.
    let exe_path = std::env::current_exe().unwrap_or_default();
    // Best-effort too (HORO-1380 S4b): `None` if hashing fails, which
    // `doctor`'s stale-runtime check treats as "nothing to report", never
    // as a mismatch.
    let exe_sha256 = libra_governor_daemon::pidfile::hash_file(&exe_path);
    let pid_record = libra_governor_daemon::pidfile::PidRecord {
        pid: std::process::id(),
        exe_path,
        started_at: current_rfc3339(),
        exe_sha256,
    };
    if let Err(e) = libra_governor_daemon::pidfile::write(&state_dir, &pid_record) {
        libra_governor_daemon::log::append_line(
            &config.log_path,
            &format!("daemon: failed to write pid record: {e}"),
        );
    }

    libra_governor_daemon::log::append_line(&config.log_path, "daemon started");
    if let Err(e) = libra_governor_daemon::serve(listener, &config) {
        libra_governor_daemon::log::append_line(
            &config.log_path,
            &format!("daemon exited with error: {e}"),
        );
        std::process::exit(1);
    }
}

fn current_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown-time".to_string())
}

/// Shells out to `kill -0 <pid>` — true iff a process with this pid exists
/// and is signalable by this user. No new dependency (`libc`/`nix`) is
/// added for this; the codebase already shells out to `pgrep`/`pkill` for
/// equivalent checks in its integration tests.
fn process_is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// True iff the process at `pid` is actually running `expected_exe` — the
/// check that makes this safe to call "identity-checked": a pid can be
/// reused by an unrelated process after the daemon that once held it
/// exited, and a stale pid record must never cause `stop` to signal
/// whatever now happens to hold that number.
fn process_exe_matches(pid: u32, expected_exe: &Path) -> bool {
    let expected = expected_exe
        .canonicalize()
        .unwrap_or_else(|_| expected_exe.to_path_buf());

    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .map(|actual| actual == expected)
            .unwrap_or(false)
    }

    #[cfg(not(target_os = "linux"))]
    {
        // No /proc on macOS/BSD: ask `ps` for the full command line and
        // compare its first token (the executable path) to the recorded
        // one. `comm=` is unreliable here — historically truncated on
        // macOS — so `command=` is used instead.
        let output = match Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
        {
            Ok(o) if o.status.success() => o,
            _ => return false,
        };
        let line = String::from_utf8_lossy(&output.stdout);
        let actual_first_token = match line.split_whitespace().next() {
            Some(tok) => tok,
            None => return false,
        };
        let actual = Path::new(actual_first_token)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(actual_first_token));
        actual == expected
    }
}

/// True iff the daemon's own socket answers `Request::Doctor` right now —
/// the third identity check, confirming the process is not merely alive
/// and correctly-named but actually the live Governor daemon (not, say, a
/// process that inherited the pid and happens to share the executable
/// path in some unusual reuse scenario).
fn socket_answers_doctor(socket_path: &Path) -> bool {
    let stream = match crate::client::connect_only(socket_path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    matches!(
        crate::client::roundtrip(&stream, Request::Doctor),
        Ok(Response::Doctor(_))
    )
}

/// `libra-governor daemon stop`: signals exactly the daemon this operator
/// started, identified by its pid record — never a broad `pkill` by name.
/// Refuses (no signal sent, non-zero exit) unless all three identity
/// checks pass: the recorded pid is alive, it is running the recorded
/// executable, and it currently answers the daemon's own socket protocol.
///
/// Returns the process exit code to use.
pub fn stop() -> i32 {
    let state_dir = match libra_governor_daemon::paths::state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("libra-governor daemon stop: could not resolve state dir: {e}");
            return 1;
        }
    };

    let record = match libra_governor_daemon::pidfile::read(&state_dir) {
        Some(r) => r,
        None => {
            eprintln!(
                "libra-governor daemon stop: no pid record at {} — either no daemon has run \
                 since this fix landed, or none is currently running. Refusing to signal \
                 anything by name.",
                state_dir.join("daemon.pid").display()
            );
            return 1;
        }
    };

    if !process_is_alive(record.pid) {
        eprintln!(
            "libra-governor daemon stop: recorded pid {} is not running — nothing to stop.",
            record.pid
        );
        return 1;
    }

    if !process_exe_matches(record.pid, &record.exe_path) {
        eprintln!(
            "libra-governor daemon stop: pid {} is alive but is not running {} — refusing to \
             signal a process that does not match the recorded daemon identity (the pid was \
             likely reused).",
            record.pid,
            record.exe_path.display()
        );
        return 1;
    }

    let socket_path = state_dir.join("daemon.sock");
    if !socket_answers_doctor(&socket_path) {
        eprintln!(
            "libra-governor daemon stop: pid {} matches the recorded executable but its socket \
             at {} is not answering — refusing to signal a process that isn't confirmed to be \
             the live daemon.",
            record.pid,
            socket_path.display()
        );
        return 1;
    }

    let killed = Command::new("kill")
        .args(["-TERM", &record.pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if killed {
        println!(
            "libra-governor daemon stop: sent SIGTERM to pid {}.",
            record.pid
        );
        0
    } else {
        eprintln!(
            "libra-governor daemon stop: all identity checks passed but signalling pid {} \
             failed.",
            record.pid
        );
        1
    }
}

#[cfg(test)]
mod stop_tests {
    use super::*;

    /// Spawns and waits for a trivial child process, returning its pid —
    /// a pid guaranteed to be dead (reaped, not just "a large number").
    /// `u32::MAX` was tried here first and is wrong on Linux: `kill`
    /// parses the pid argument into a `pid_t` (32-bit signed), and
    /// `4294967295` truncates to `-1`, which `kill(2)` treats specially —
    /// "signal every process this caller may signal" — so `kill -0 -1`
    /// succeeds as long as any such process exists, making
    /// `process_is_alive` wrongly report `true`. A real daemon's pid is
    /// always a small positive number well inside `pid_t`'s range, so
    /// this reaped-child pid is a realistic stand-in for "was alive, now
    /// is not" rather than an out-of-range value that doesn't model a
    /// real pid at all.
    fn dead_pid() -> u32 {
        let mut child = Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("failed to spawn `sh -c exit 0`");
        let pid = child.id();
        child.wait().expect("failed to wait for the child");
        pid
    }

    #[test]
    fn process_is_alive_is_false_for_a_pid_that_cannot_exist() {
        assert!(!process_is_alive(dead_pid()));
    }

    #[test]
    fn process_exe_matches_is_false_when_recorded_path_does_not_exist() {
        // This process (the test binary) is alive, but its exe never
        // equals a path that does not exist on disk.
        let pid = std::process::id();
        assert!(!process_exe_matches(
            pid,
            Path::new("/definitely/not/a/real/executable/path")
        ));
    }

    #[test]
    fn socket_answers_doctor_is_false_when_nothing_is_listening() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!socket_answers_doctor(&dir.path().join("nothing.sock")));
    }

    #[test]
    fn stop_refuses_when_no_pid_record_exists() {
        let _guard = super::state_dir_env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LIBRA_GOVERNOR_STATE_DIR", dir.path());
        let exit_code = stop();
        std::env::remove_var("LIBRA_GOVERNOR_STATE_DIR");
        assert_eq!(exit_code, 1);
    }

    #[test]
    fn stop_refuses_when_recorded_pid_is_not_alive() {
        let _guard = super::state_dir_env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LIBRA_GOVERNOR_STATE_DIR", dir.path());
        libra_governor_daemon::pidfile::write(
            dir.path(),
            &libra_governor_daemon::pidfile::PidRecord {
                pid: dead_pid(),
                exe_path: std::path::PathBuf::from("/bin/does-not-matter"),
                started_at: "2026-09-27T00:00:00Z".to_string(),
                exe_sha256: None,
            },
        )
        .unwrap();
        let exit_code = stop();
        std::env::remove_var("LIBRA_GOVERNOR_STATE_DIR");
        assert_eq!(exit_code, 1);
    }

    #[test]
    fn stop_refuses_when_pid_is_alive_but_exe_does_not_match() {
        let _guard = super::state_dir_env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LIBRA_GOVERNOR_STATE_DIR", dir.path());
        // This test process is alive, but its recorded exe path is wrong,
        // so `stop` must refuse to signal it — this is the exact boundary
        // HORO-1380 exists to enforce: a live, correctly-owned pid is not
        // enough on its own.
        libra_governor_daemon::pidfile::write(
            dir.path(),
            &libra_governor_daemon::pidfile::PidRecord {
                pid: std::process::id(),
                exe_path: std::path::PathBuf::from("/definitely/not/this/test/binary"),
                started_at: "2026-09-27T00:00:00Z".to_string(),
                exe_sha256: None,
            },
        )
        .unwrap();
        let exit_code = stop();
        std::env::remove_var("LIBRA_GOVERNOR_STATE_DIR");
        assert_eq!(exit_code, 1);
    }
}
