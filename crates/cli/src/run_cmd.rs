//! `libra-governor run --pool <alias> [--max-wait <secs>] -- <command...>`
//! (HORO-1766 PR2): the one launcher/queue wrapper that applies the
//! launch-gate's pacing (PR1, `libra_governor_daemon::launch_gate`) to a
//! real subprocess. See `docs/adr/0020-launch-gate-actuator.md` for the
//! accepted design this module implements exactly.
//!
//! # Lifecycle
//!
//! 1. Ensure a daemon exists (spawn-if-absent, via
//!    [`crate::client::ensure_daemon_connection`]).
//! 2. `Admit` in a loop under one freshly minted `launch_id`: `Wait` means
//!    retry after the server's hint (optionally bounded by `--max-wait`);
//!    `Unavailable` means fail closed, never spawning anything
//!    ([`EXIT_NOT_ADMITTED`]).
//! 3. On `Start`, spawn the real command: full inherited environment, no
//!    new process group, stdio untouched.
//! 4. While the child runs: `Renew` the hold roughly every 60s, and pin
//!    the child's identity the `waitid(..., NOWAIT)` way so this process
//!    can signal and later reap exactly the one process it started.
//! 5. `Release` the hold (as soon as the child is observed to have
//!    exited, before the final reap) on every normal exit path, via a
//!    drop guard so an early return still releases.
//!
//! # What this deliberately does not reuse
//!
//! `libra_governor_daemon::host_runtime::exec`'s `Cancellation` type
//! exists and is `pub` from this crate's point of view
//! (`host_runtime::dispatch::Cancellation`, already used by
//! `adapter_probe_signal.rs`), but its `cancelled()` reader is
//! `pub(super)` — visible inside `libra-governor-daemon`'s own
//! `host_runtime` module tree, not across the crate boundary to `cli`.
//! This module therefore cannot hold a `Cancellation` and read it here;
//! it reuses the *pattern* instead (a signal handler that only flips a
//! `'static AtomicBool`, read elsewhere, never a library-installed
//! `SIG_IGN`) with its own local static, rather than the type itself.
//!
//! Also deliberately not reused: `daemon_cmd::stop`'s
//! identity-checked-kill (pid + `ps -o command=` executable-path
//! comparison) — spoofable via `argv[0]` and racy (check-then-signal).
//! The child here is pinned by `waitid(..., WNOWAIT)` on the exact pid
//! this process itself forked, a strictly stronger identity proof that
//! needs no separate check at signal time.

use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::process::{self, Pid, Signal, WaitId, WaitIdOptions};
use uuid::Uuid;

use libra_governor_domain::SharedPoolReservationId;
use libra_governor_protocol::{LaunchOutcome, LaunchRequest, Request, Response};

use crate::client::{self, ClientError};

/// Fail-closed exit code for every path that refuses to spawn at all
/// (ADR-0020's explicit default for an unadmitted launch).
pub const EXIT_NOT_ADMITTED: i32 = 75;
/// Argument-parsing failure, or the child's own executable could not be
/// started (distinct from the child's *own* exit code, which is passed
/// through unchanged).
const EXIT_USAGE: i32 = 2;
const EXIT_SPAWN_FAILED: i32 = 126;

/// Renew cadence (design: "every ~60s" — a fixed cadence, not derived
/// from PR1's configured `hold_ttl_secs`, which defaults to 300s and
/// exists only as the outer safety margin a missed renewal eats into).
const RENEW_INTERVAL: Duration = Duration::from_secs(60);
/// How often the main loop polls the child's exit state and signal
/// flags while it runs.
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// How often a `Wait` outcome prints a status line — not every retry.
const WAIT_STATUS_INTERVAL: Duration = Duration::from_secs(30);

/// Set only by [`on_signal`] for `SIGTERM`/`SIGHUP` — read by the main
/// loop to forward exactly one `SIGTERM` to the pinned child pid. Never
/// read directly by anything outside this module.
static TERMINATE_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(signal: libc::c_int) {
    if signal == libc::SIGTERM || signal == libc::SIGHUP {
        TERMINATE_REQUESTED.store(true, Ordering::SeqCst);
    }
    // SIGINT: deliberately no action. The terminal already delivers it to
    // the child directly (same process group, see `spawn_child`'s docs) —
    // this handler exists only so this process's own disposition is a
    // real function, never `SIG_IGN` (which would survive `execve` and be
    // inherited as ignored by a *subsequently forked* child; here it
    // matters only for correctness-by-construction, since `SIGINT`'s own
    // disposition never gates anything in this module).
}

/// Installs real (non-`SIG_IGN`) handlers for `SIGINT`/`SIGHUP`/`SIGTERM`.
/// Must run before the child is spawned (ADR-0020 design §7): a
/// `SIG_IGN` disposition survives `execve` and would be inherited by the
/// spawned command, silently defeating its own Ctrl-C handling. A real
/// handler function does not have this problem — `execve` resets any
/// signal with a non-default, non-ignored handler back to `SIG_DFL`
/// automatically, with no extra action required here; see
/// `crates/daemon/src/host_runtime/exec.rs`'s own child-side
/// `sigaction(..., default_signal, ...)` reset for the same guarantee
/// applied explicitly rather than relying on `execve`'s semantics (that
/// module forks and `exec`s by hand; this one uses
/// `std::process::Command`, which does not expose a pre-exec signal-mask
/// reset hook, so this module leans on the POSIX guarantee instead).
fn install_signal_handlers() -> std::io::Result<()> {
    for signal in [libc::SIGINT, libc::SIGHUP, libc::SIGTERM] {
        // SAFETY: `action` is fully initialized before being passed to
        // `sigaction`; `on_signal` only ever stores to a `'static`
        // `AtomicBool`, which is async-signal-safe.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as usize;
            if libc::sigemptyset(&mut action.sa_mask) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }

    // The whole zombie-preservation argument for pid-pinning (module
    // doc, `run`'s step 4/6 loop) depends on the spawned child staying a
    // reapable zombie after it exits. POSIX `SIG_IGN` on `SIGCHLD`
    // (unlike a real handler) survives `execve`, so if this process
    // inherited that disposition from whatever invoked it (a shell/
    // supervisor with job control off, `trap '' CHLD`, etc.), the kernel
    // auto-reaps the child the instant it exits and `waitid(..., NOWAIT)`
    // below never observes the exit — a permanent hang that also leaks
    // the held slot forever via the renew loop. Reset to `SIG_DFL`
    // explicitly rather than trusting the ambient disposition.
    //
    // SAFETY: same as above — a plain `sigaction` call, no async-signal-
    // unsafe state touched.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        if libc::sigemptyset(&mut action.sa_mask) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    Ok(())
}

/// Writes one status/error line to stderr without ever panicking — never
/// `eprintln!` in this module's hot lifecycle path (design §9): a panic
/// on `EIO` after a terminal hangup would unwind past the release guard
/// only if the unwind itself aborts the process, but the safer discipline
/// is simply never to give a write failure the chance to panic at all.
fn status_line(message: &str) {
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "libra-governor run: {message}");
    let _ = stderr.flush();
}

struct ParsedArgs {
    pool_alias: String,
    max_wait: Option<Duration>,
    command: Vec<String>,
}

fn parse_args(args: &[String]) -> Result<ParsedArgs, String> {
    let mut pool_alias: Option<String> = None;
    let mut max_wait: Option<Duration> = None;
    let mut index = 0;
    let mut saw_separator = false;
    while index < args.len() {
        match args[index].as_str() {
            "--pool" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or("--pool requires a value")?
                    .to_string();
                pool_alias = Some(value);
            }
            "--max-wait" => {
                index += 1;
                let raw = args.get(index).ok_or("--max-wait requires a value")?;
                let secs: u64 = raw.parse().map_err(|_| {
                    format!("--max-wait value {raw:?} is not a whole number of seconds")
                })?;
                max_wait = Some(Duration::from_secs(secs));
            }
            "--" => {
                index += 1;
                saw_separator = true;
                break;
            }
            other => return Err(format!("unrecognized argument {other:?} (expected `--`)")),
        }
        index += 1;
    }
    let command = args[index..].to_vec();
    let pool_alias = pool_alias.ok_or("--pool <alias> is required")?;
    if !saw_separator || command.is_empty() {
        return Err("expected `-- <command> [args...]` after the launcher options".to_string());
    }
    Ok(ParsedArgs {
        pool_alias,
        max_wait,
        command,
    })
}

fn connect_and_roundtrip(socket_path: &Path, request: Request) -> Result<Response, ClientError> {
    let stream = client::connect_only(socket_path)?;
    client::roundtrip(&stream, request)
}

fn release(socket_path: &Path, reservation_id: SharedPoolReservationId, launch_id: Uuid) {
    // Best-effort: a failed release just means the hold is reclaimed at
    // TTL expiry instead of immediately (ADR-0020 §5) — never worth
    // panicking or retrying indefinitely over, since the child has
    // already exited by the time this is called.
    let _ = connect_and_roundtrip(
        socket_path,
        Request::Launch {
            launch: LaunchRequest::Release {
                reservation_id,
                launch_id,
            },
        },
    );
}

/// Releases the held slot exactly once, on every exit path — including an
/// early `return` from inside [`run`] — via `Drop`.
struct ReleaseGuard<'a> {
    socket_path: &'a Path,
    reservation_id: SharedPoolReservationId,
    launch_id: Uuid,
    released: bool,
}

impl ReleaseGuard<'_> {
    fn release_now(&mut self) {
        if !self.released {
            release(self.socket_path, self.reservation_id, self.launch_id);
            self.released = true;
        }
    }
}

impl Drop for ReleaseGuard<'_> {
    fn drop(&mut self) {
        self.release_now();
    }
}

fn exit_code_for_status(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        code
    } else if let Some(signal) = status.signal() {
        128 + signal
    } else {
        1
    }
}

/// `libra-governor run --pool <alias> [--max-wait <secs>] -- <command...>`.
/// Returns the process exit code to use.
pub fn run(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(message) => {
            eprintln!("libra-governor run: {message}");
            return EXIT_USAGE;
        }
    };

    let socket_path = match libra_governor_daemon::paths::socket_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("libra-governor run: could not resolve state dir: {e}");
            return EXIT_NOT_ADMITTED;
        }
    };

    // Step 1: ensure a daemon exists at all (spawn-if-absent). A failure
    // here means there is no one to even ask, so this fails closed the
    // same way an explicit `Unavailable` would.
    if let Err(e) = client::ensure_daemon_connection(&socket_path) {
        status_line(&format!(
            "not admitted: daemon unavailable ({e}) -- refusing to spawn (fail closed)"
        ));
        return EXIT_NOT_ADMITTED;
    }

    let launch_id = Uuid::new_v4();
    let admit_deadline = parsed.max_wait.map(|budget| Instant::now() + budget);
    let mut last_wait_print: Option<Instant> = None;

    let (reservation_id, _renew_every_secs) = loop {
        let outcome = match connect_and_roundtrip(
            &socket_path,
            Request::Launch {
                launch: LaunchRequest::Admit {
                    pool_alias: parsed.pool_alias.clone(),
                    launch_id,
                },
            },
        ) {
            Ok(Response::Launch(outcome)) => *outcome,
            Ok(other) => {
                status_line(&format!(
                    "not admitted: unexpected daemon response ({other:?}) -- refusing to spawn \
                     (fail closed)"
                ));
                return EXIT_NOT_ADMITTED;
            }
            Err(e) => {
                status_line(&format!(
                    "not admitted: daemon unreachable ({e}) -- refusing to spawn (fail closed)"
                ));
                return EXIT_NOT_ADMITTED;
            }
        };

        match outcome {
            LaunchOutcome::Start {
                reservation_id,
                renew_every_secs,
                ..
            } => break (reservation_id, renew_every_secs),
            LaunchOutcome::Wait {
                reason,
                retry_after_secs,
            } => {
                if let Some(deadline) = admit_deadline {
                    if Instant::now() >= deadline {
                        status_line(&format!(
                            "not admitted: timed out waiting for pool {:?} ({reason:?}) -- \
                             refusing to spawn (fail closed)",
                            parsed.pool_alias
                        ));
                        return EXIT_NOT_ADMITTED;
                    }
                }
                let now = Instant::now();
                let should_print = last_wait_print
                    .map(|t| now.duration_since(t) >= WAIT_STATUS_INTERVAL)
                    .unwrap_or(true);
                if should_print {
                    status_line(&format!(
                        "waiting for a slot in pool {:?} ({reason:?})",
                        parsed.pool_alias
                    ));
                    last_wait_print = Some(now);
                }
                std::thread::sleep(Duration::from_secs(retry_after_secs.max(1)));
            }
            LaunchOutcome::Unavailable { reason } => {
                status_line(&format!(
                    "not admitted: {reason:?} -- refusing to spawn (fail closed)"
                ));
                return EXIT_NOT_ADMITTED;
            }
            other => {
                status_line(&format!(
                    "not admitted: unexpected admit outcome ({other:?}) -- refusing to spawn \
                     (fail closed)"
                ));
                return EXIT_NOT_ADMITTED;
            }
        }
    };

    let mut guard = ReleaseGuard {
        socket_path: &socket_path,
        reservation_id,
        launch_id,
        released: false,
    };

    // Step 5: install signal handlers BEFORE spawning the child.
    if let Err(e) = install_signal_handlers() {
        status_line(&format!(
            "could not install signal handlers ({e}); refusing to spawn (fail closed)"
        ));
        guard.release_now();
        return EXIT_NOT_ADMITTED;
    }

    // Step 3: spawn the child. Deliberately no `.env_clear()` (inherit the
    // full environment byte-for-byte, AC3) and no `.process_group(..)`
    // call (stay in this process's own group, so Ctrl-Z/Ctrl-C behave
    // exactly as if the command ran directly — `std::process::Command`
    // does not create a new process group unless explicitly asked to).
    let mut command = Command::new(&parsed.command[0]);
    command.args(&parsed.command[1..]);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            status_line(&format!("failed to start {:?}: {e}", parsed.command[0]));
            guard.release_now();
            return EXIT_SPAWN_FAILED;
        }
    };
    let Some(child_pid) = Pid::from_raw(child.id() as i32) else {
        status_line("spawned child reported pid 0 -- treating as a fatal setup failure");
        let _ = child.kill();
        let _ = child.wait();
        guard.release_now();
        return EXIT_SPAWN_FAILED;
    };

    let mut last_renew = Instant::now();
    let mut term_forwarded = false;
    let mut enforcement_lost = false;

    // Step 4/6: poll the pinned child's exit state without reaping it,
    // forward a single SIGTERM on cancel intent, and renew the hold
    // roughly every 60s.
    loop {
        match process::waitid(
            WaitId::Pid(child_pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ) {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => {
                // An error here (e.g. ECHILD, which is exactly what a
                // reset-but-still-inherited reaping race would produce)
                // means this process can no longer prove the child is
                // still its own un-reaped zombie — the whole pid-pinning
                // safety argument has broken down. Never fold this into
                // "still running": that is how a `SIGCHLD=SIG_IGN`-style
                // ambient disposition turns into a permanent hang and a
                // permanently leaked slot. Treat it as terminal instead.
                status_line(&format!(
                    "lost the ability to observe the job's exit state via waitid ({e}); \
                     releasing the held slot and exiting without sending further signals to \
                     pid {child_pid} (its identity can no longer be verified)"
                ));
                guard.release_now();
                return 1;
            }
        }

        if TERMINATE_REQUESTED.load(Ordering::SeqCst) && !term_forwarded {
            // Signal exactly the pinned pid, never a process group — a
            // `Release`d/stale pid is never reused here because `waitid`
            // with `WNOWAIT` has kept this exact process un-reaped the
            // whole time.
            let _ = process::kill_process(child_pid, Signal::TERM);
            term_forwarded = true;
            status_line(
                "cancel requested; forwarded SIGTERM to the running job (no SIGKILL escalation \
                 -- waiting for it to exit on its own)",
            );
        }

        if last_renew.elapsed() >= RENEW_INTERVAL {
            let renewed = matches!(
                connect_and_roundtrip(
                    &socket_path,
                    Request::Launch {
                        launch: LaunchRequest::Renew {
                            reservation_id,
                            launch_id,
                        },
                    },
                ),
                Ok(Response::Launch(outcome)) if matches!(*outcome, LaunchOutcome::Renewed { .. })
            );
            if !renewed && !enforcement_lost {
                status_line(
                    "EnforcementLost: could not renew the held slot -- the job keeps running, \
                     unenforced; a fresh `run` invocation is still subject to admission",
                );
                enforcement_lost = true;
            }
            last_renew = Instant::now();
        }

        std::thread::sleep(POLL_INTERVAL);
    }

    // Step 6: release BEFORE the final reap (design's explicit ordering).
    guard.release_now();

    // Final blocking reap: the child has already exited (confirmed by
    // `waitid` above), so this returns immediately rather than blocking.
    let status = match child.wait() {
        Ok(status) => status,
        Err(e) => {
            status_line(&format!("failed to reap the exited child: {e}"));
            return 1;
        }
    };

    let code = exit_code_for_status(&status);
    status_line(&format!(
        "job exited (code {code}){}{}",
        if term_forwarded {
            "; cancellation was requested via SIGTERM"
        } else {
            ""
        },
        if enforcement_lost {
            "; the hold was not continuously enforced during this run"
        } else {
            ""
        },
    ));
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_requires_pool_and_a_command_after_the_separator() {
        assert!(parse_args(&[]).is_err());
        assert!(parse_args(&["--pool".to_string()]).is_err());
        assert!(parse_args(&["--pool".to_string(), "agents".to_string()]).is_err());
        assert!(
            parse_args(&["--pool".to_string(), "agents".to_string(), "--".to_string()]).is_err()
        );
    }

    #[test]
    fn parse_args_accepts_max_wait_and_a_command() {
        let parsed = parse_args(&[
            "--pool".to_string(),
            "agents".to_string(),
            "--max-wait".to_string(),
            "30".to_string(),
            "--".to_string(),
            "echo".to_string(),
            "hi".to_string(),
        ])
        .unwrap();
        assert_eq!(parsed.pool_alias, "agents");
        assert_eq!(parsed.max_wait, Some(Duration::from_secs(30)));
        assert_eq!(parsed.command, vec!["echo".to_string(), "hi".to_string()]);
    }

    #[test]
    fn exit_code_passes_through_a_normal_exit_code() {
        let status = std::process::Command::new("sh")
            .args(["-c", "exit 7"])
            .status()
            .unwrap();
        assert_eq!(exit_code_for_status(&status), 7);
    }

    #[test]
    fn exit_code_is_128_plus_signal_for_a_signal_kill() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = Pid::from_raw(child.id() as i32).unwrap();
        process::kill_process(pid, Signal::TERM).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(exit_code_for_status(&status), 128 + libc::SIGTERM);
    }
}
