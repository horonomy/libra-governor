//! The one-shot stdio subprocess runner (HORO-1853 design-v2 transport).
//!
//! Spawns the exact approved executable with fixed argv and a fixed
//! two-variable environment, writes one request frame to stdin, closes
//! stdin, reads one response frame from stdout until EOF, and waits for
//! exit code 0 — all bounded by one deadline covering start, write,
//! read, and exit together. On timeout or cancellation, the *owned
//! process group* is killed (not just the direct child), so a
//! misbehaving extension that backgrounds a grandchild holding stdout
//! open cannot hang the host past the deadline.
//!
//! # Group-kill is not full containment
//!
//! Killing the owned process group reaches every descendant that stays
//! *in* the group. It is not a sandbox: a descendant that calls
//! `setsid()` (or otherwise detaches itself from the group) survives
//! the kill by design — nothing in this module can reach it, since the
//! only identifier this code kills by is the group id. The guarantee
//! this module actually provides is narrower than "nothing outlives the
//! deadline": it is that `invoke()` itself always returns on time
//! regardless of what an escaped descendant does afterward. See
//! `run_one_shot`'s tail (the bounded-join section) for how that is
//! kept true even when a reader thread is wedged forever on a pipe an
//! escaped process still holds open.
//!
//! # The one load-bearing ordering rule
//!
//! `kill_process_group` must never be called after this process has
//! already been reaped (by `try_wait`/`wait` returning `Some`/`Ok`).
//! Once reaped, the OS is free to recycle the PID — and since the
//! killed group is identified by that same numeric value (`-pid`), a
//! kill issued afterward could hit an unrelated, unrelated-to-us
//! process group that happens to have been assigned the same id. This
//! module's control flow is written so every kill happens strictly
//! before the single reaping call for that child; macOS/Linux only, no
//! Windows process-group support is attempted.
//!
//! # Deadline budget
//!
//! `deadline` bounds start + write + read + exit. On top of that,
//! `invoke()`/`run_one_shot()` can take up to two further bounded grace
//! waits of [`JOIN_GRACE`] each (one for the writer thread, one for the
//! reader thread) while deciding whether it is safe to join each
//! helper thread — see the tail of `run_one_shot`. So the real worst
//! case before this function returns is `deadline + 2 * JOIN_GRACE`,
//! not `deadline` alone. Neither grace wait is spent joining a thread
//! that is still blocked on an escaped descendant's pipe — see the
//! module docs above — so this bound holds even against the setsid
//! attack a regression test in this module exercises.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::de::DeserializeOwned;

use crate::identity::{self, IdentityError};
use crate::registration::{Registration, RegistrationError};

/// Request frame size cap (HORO-1853 design-v2).
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
/// Response frame size cap (HORO-1853 design-v2).
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("request frame of {0} bytes exceeds the {MAX_REQUEST_BYTES}-byte cap")]
    RequestTooLarge(usize),
    #[error("failed to spawn {0:?}: {1}")]
    Spawn(String, String),
    #[error("deadline exceeded")]
    Timeout,
    #[error("cancelled")]
    Cancelled,
    #[error("response exceeded the {MAX_RESPONSE_BYTES}-byte cap")]
    ResponseTooLarge,
    #[error("i/o error: {0}")]
    Io(String),
    #[error("process exited with a non-zero or signalled status: {0:?}")]
    NonZeroExit(Option<i32>),
}

/// Error from the crate's one public entry point, [`invoke`].
#[derive(Debug, thiserror::Error)]
pub enum InvokeError {
    #[error("registration is invalid: {0}")]
    Registration(#[from] RegistrationError),
    #[error("identity verification failed: {0}")]
    Identity(#[from] IdentityError),
    #[error("run failed: {0}")]
    Run(#[from] RunError),
}

/// The crate's sole public entry point for executing a registered
/// extension. In order, every call: (1) re-validates the registration's
/// own structural/semantic invariants, (2) re-measures and verifies its
/// code identity against the bound approval digests
/// ([`identity::verify_identity`]), and only then (3) runs the one-shot
/// stdio transport. `run_one_shot` itself is a private implementation
/// detail precisely so that there is no way to reach it from outside
/// this crate without going through steps (1) and (2) first — the
/// re-measure-before-every-invocation guarantee is enforced by this
/// function being the only path in, not by caller discipline.
///
/// # Precondition this function does not itself check
///
/// Cross-registration `(account_fingerprint, pool_alias)` uniqueness
/// ([`crate::registration::load_registrations`]'s
/// `check_pool_uniqueness`) is the *loader's* responsibility, not this
/// function's: `invoke` only ever sees the one [`Registration`] it was
/// called with, so it has no way to detect a duplicate pool binding
/// declared in a sibling registration file. Callers must go through
/// `load_registrations` (or re-run the same uniqueness check
/// themselves) before calling `invoke` on registrations assembled any
/// other way.
pub fn invoke(
    registration: &Registration,
    request: &[u8],
    cancel: &AtomicBool,
) -> Result<Vec<u8>, InvokeError> {
    registration.validate()?;
    identity::verify_identity(registration)?;
    let deadline = Duration::from_millis(registration.timeout_ms);
    let frame = run_one_shot(
        &registration.transport.executable,
        &registration.transport.args,
        request,
        deadline,
        cancel,
    )?;
    Ok(frame)
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("response frame is empty")]
    Empty,
    #[error("response frame did not parse as a single JSON value: {0}")]
    Invalid(String),
    #[error("response frame has trailing bytes after the first JSON value")]
    TrailingOutput,
}

/// Parses exactly one JSON frame from `bytes`: a single JSON value,
/// optionally followed by one trailing `\n` and nothing else. Any other
/// trailing content — a second frame, unterminated garbage, anything —
/// is rejected; `serde_json::from_slice` alone would silently accept
/// trailing whitespace, which this protocol does not allow.
pub fn parse_single_frame<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, FrameError> {
    let mut stream = serde_json::Deserializer::from_slice(bytes).into_iter::<T>();
    let value = match stream.next() {
        Some(Ok(value)) => value,
        Some(Err(error)) => return Err(FrameError::Invalid(error.to_string())),
        None => return Err(FrameError::Empty),
    };
    let offset = stream.byte_offset();
    let trailing = &bytes[offset..];
    if !(trailing.is_empty() || trailing == b"\n") {
        return Err(FrameError::TrailingOutput);
    }
    Ok(value)
}

#[cfg(unix)]
fn kill_owned_process_group(pid: u32) {
    use rustix::process::{kill_process_group, Pid, Signal};
    if let Some(pid) = Pid::from_raw(pid as i32) {
        let _ = kill_process_group(pid, Signal::KILL);
    }
}

/// Bound on each of the two post-kill joins (writer, reader) in
/// `run_one_shot`'s tail. See the module-level "Deadline budget" docs:
/// the real worst-case wall time before `run_one_shot` returns is
/// `deadline + 2 * JOIN_GRACE`, not `deadline` alone.
const JOIN_GRACE: Duration = Duration::from_millis(1_000);

/// Serializes this crate's own `Command::spawn()` calls against each
/// other. `std::process::Command` creates the stdio pipe file
/// descriptors and only marks them close-on-exec *after* `fork()`
/// returns in the parent — there is a real window, between pipe
/// creation and that flag being set, during which a `spawn()` running
/// concurrently on another thread can `fork()` and inherit an
/// in-flight invocation's pipe fds into an unrelated child. Observed in
/// practice (HORO-1854 adversarial review): with two concurrent
/// invocations, a stray byte sequence from one child's output showed up
/// in a frame read for a different invocation. Holding this lock only
/// across the `spawn()` call (not the whole `run_one_shot`) serializes
/// just the fork/exec moment, which is all that is actually racy; it
/// does not protect against some other, unrelated piece of code in the
/// same host process also spawning children outside this crate — that
/// remains a host-wide concern out of this crate's scope.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// Runs `executable` as a one-shot stdio child: fixed argv, an
/// environment containing **only** `LANG=C.UTF-8`/`LC_ALL=C.UTF-8`
/// (`env_clear` first — no ambient inheritance of any kind, including
/// `HOME` and any credential-shaped variable), stderr discarded. Writes
/// `request` to stdin then closes it, reads at most
/// [`MAX_RESPONSE_BYTES`] + 1 bytes from stdout, and requires exit code
/// 0. `deadline` bounds the whole operation (start + write + read +
/// exit); `cancel` is polled throughout and, like a deadline expiry,
/// triggers an owned-process-group kill rather than merely dropping the
/// `Child` handle (which would leak the process).
///
/// Private: see [`invoke`] for why this is never reachable from outside
/// the crate on its own.
fn run_one_shot(
    executable: &str,
    args: &[String],
    request: &[u8],
    deadline: Duration,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, RunError> {
    if request.len() > MAX_REQUEST_BYTES {
        return Err(RunError::RequestTooLarge(request.len()));
    }

    // The deadline covers start + write + read + exit together, per
    // spec — computed before `spawn()`, not after, so that even the
    // process creation itself is inside the bound.
    let deadline_at = Instant::now() + deadline;

    let mut command = Command::new(executable);
    command
        .args(args)
        .env_clear()
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        // The child must never silently run with whatever directory the
        // host process happened to have as its own cwd (which may be a
        // path chosen for, or meaningful to, something else entirely).
        // "/" always exists and is never writable by an unprivileged
        // extension, so this is a safe, deterministic default.
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        // New, independent process group led by this child, so a
        // timeout/cancel kill can reach every descendant it spawns —
        // not just the direct child — without also hitting our own
        // process group.
        command.process_group(0);
    }

    let mut child = {
        // See [`SPAWN_LOCK`]: held only across the fork/exec call itself.
        let _spawn_guard = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        command
            .spawn()
            .map_err(|error| RunError::Spawn(executable.to_owned(), error.to_string()))?
    };
    let pid = child.id();

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let mut stdout = child.stdout.take().expect("stdout was piped");

    let request_owned = request.to_vec();
    let (write_tx, write_rx) = mpsc::channel();
    let writer = thread::spawn(move || {
        let result = stdin.write_all(&request_owned).map_err(|e| e.to_string());
        let _ = write_tx.send(result);
    });

    let (read_tx, read_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut limited = (&mut stdout).take(MAX_RESPONSE_BYTES as u64 + 1);
        let mut buf = Vec::new();
        let result = limited
            .read_to_end(&mut buf)
            .map(|_| buf)
            .map_err(|e| e.to_string());
        let _ = read_tx.send(result);
    });

    let poll_step = Duration::from_millis(10);

    // Kills the owned process group and reaps exactly once via a
    // blocking `wait()`. Only ever called while `child` is confirmed
    // *not yet reaped* — an exited-but-unreaped leader is a zombie,
    // which keeps its PID (and therefore its process group id)
    // reserved, so `killpg` against it is still safe even though it
    // has already exited. This is what makes it safe to call from
    // Phase 1 below, where the leader itself may already be a zombie
    // while a grandchild it backgrounded is still very much alive and
    // holding stdout open.
    let kill_and_reap = |child: &mut std::process::Child| -> std::process::ExitStatus {
        #[cfg(unix)]
        kill_owned_process_group(pid);
        // `wait()` on an already-exited-but-unreaped child returns
        // immediately with its real status; it does not block waiting
        // for a group member we just killed to separately exit, since
        // `wait()` here only reaps *this* `Child` handle's own pid.
        child.wait().unwrap_or_else(|_| {
            // Reaping itself failing is exceedingly unlikely (the pid
            // is definitely ours); fall back to a status object that
            // reports failure rather than panicking.
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                std::process::ExitStatus::from_raw(1)
            }
            #[cfg(not(unix))]
            {
                std::process::exit(1)
            }
        })
    };

    // Phase 1: wait for a complete response frame (reader EOF/cap/err),
    // or for a timeout/cancel — deliberately *not* calling `try_wait`
    // here. A misbehaving extension whose leader exits immediately but
    // leaves a backgrounded grandchild holding stdout open must still
    // be caught by the deadline: reaping the leader early and declaring
    // victory the moment it exits, while that grandchild silently lives
    // on past the deadline, is exactly the hang this transport exists
    // to prevent.
    let mut abort_reason: Option<RunError> = None;
    let read_result = loop {
        if cancel.load(Ordering::SeqCst) {
            abort_reason = Some(RunError::Cancelled);
            break None;
        }
        let remaining = deadline_at.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            abort_reason = Some(RunError::Timeout);
            break None;
        }
        match read_rx.recv_timeout(remaining.min(poll_step)) {
            Ok(result) => break Some(result),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Some(Err("reader thread disconnected".into()));
            }
        }
    };

    // An over-cap frame is itself a reason to abort and kill now,
    // before Phase 2: otherwise a flooding child whose writer thread is
    // still blocked on a full pipe would never exit on its own, and
    // Phase 2's exit-wait would simply burn the rest of the deadline
    // and return `Timeout` instead of `ResponseTooLarge`.
    if abort_reason.is_none() {
        if let Some(Ok(bytes)) = &read_result {
            if bytes.len() > MAX_RESPONSE_BYTES {
                abort_reason = Some(RunError::ResponseTooLarge);
            }
        }
    }

    let status = if abort_reason.is_some() {
        kill_and_reap(&mut child)
    } else {
        // A complete (or over-cap, or errored) frame arrived. The
        // leader may or may not have exited yet; bound the wait for
        // its exit by whatever deadline remains.
        let mut reaped_status = None;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    reaped_status = Some(status);
                    break;
                }
                Ok(None) => {}
                Err(error) => {
                    abort_reason = Some(RunError::Io(error.to_string()));
                    break;
                }
            }
            if cancel.load(Ordering::SeqCst) {
                abort_reason = Some(RunError::Cancelled);
                break;
            }
            if Instant::now() >= deadline_at {
                abort_reason = Some(RunError::Timeout);
                break;
            }
            thread::sleep(poll_step);
        }
        // KNOWN LIMITATION, deliberately not fixed in this pass (HORO-1854
        // adversarial review, should-fix item "success-path stragglers
        // survive"): if the leader exits 0 on its own, `try_wait` above
        // reaps it directly, with no group-kill beforehand — so a
        // backgrounded grandchild the leader spawned (e.g. `sleep 30 &`)
        // is never killed and outlives `invoke()` returning `Ok`. The
        // correct fix is to detect exit without reaping (e.g.
        // `waitid(WEXITED | WNOHANG | WNOWAIT)`), kill the group, and
        // only then perform the single reaping `wait()` — but that
        // changes this function's one load-bearing ordering rule (see
        // the module docs) and deserves its own focused change with
        // platform verification on both CI targets (macOS and Linux),
        // not a rushed edit alongside unrelated fixes. Tracked as a
        // follow-up; not landed here.
        match reaped_status {
            Some(status) => status,
            None => kill_and_reap(&mut child),
        }
    };

    // The process group is now dead (or exited on its own), so the
    // pipes should close promptly in the overwhelmingly common case.
    // But never call an unconditional `join()`: a descendant that
    // escaped the group (e.g. via `setsid()`) can hold a pipe open
    // indefinitely, and a thread blocked reading or writing that pipe
    // would then block this join forever — defeating the whole point
    // of the deadline (see the module's "Group-kill is not full
    // containment" docs). So each helper thread is joined *only* when
    // its channel recv has already told us the thread is done (it sent
    // its result and is finishing, not still blocked in I/O); otherwise
    // the handle is dropped and the thread is left running detached.
    // This bounds the extra time this function can spend here to at
    // most `2 * JOIN_GRACE` beyond `deadline` (see the module's
    // "Deadline budget" docs), regardless of what an escaped descendant
    // does afterward.
    let write_result = write_rx.recv_timeout(JOIN_GRACE);
    if write_result.is_ok() {
        let _ = writer.join();
    }

    let read_result = match read_result {
        Some(result) => {
            // Phase 1 already received a result on this channel, so the
            // reader thread already sent and is finishing (or has
            // finished) on its own — safe to join without risking a
            // block on an escaped descendant's pipe.
            let _ = reader.join();
            result
        }
        None => match read_rx.recv_timeout(JOIN_GRACE) {
            Ok(result) => {
                let _ = reader.join();
                result
            }
            Err(_) => Err("stdout read did not complete".into()),
        },
    };

    if let Some(reason) = abort_reason {
        return Err(reason);
    }

    let frame = read_result.map_err(RunError::Io)?;
    if frame.len() > MAX_RESPONSE_BYTES {
        return Err(RunError::ResponseTooLarge);
    }
    // A write failure (e.g. the child closed stdin early without
    // reading all of it) is not itself fatal if the child still
    // produced a complete frame and exited successfully — some
    // extensions may not need the full request. It is surfaced only
    // when nothing useful came back.
    if !status.success() {
        return Err(RunError::NonZeroExit(exit_code(&status)));
    }
    if frame.is_empty() {
        if let Ok(Err(error)) = write_result {
            return Err(RunError::Io(error));
        }
    }
    Ok(frame)
}

#[cfg(unix)]
fn exit_code(status: &std::process::ExitStatus) -> Option<i32> {
    status.code()
}

#[cfg(not(unix))]
fn exit_code(status: &std::process::ExitStatus) -> Option<i32> {
    status.code()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// Rust's default test harness runs every test in this file in
    /// parallel. Most of the tests below spawn a real child process and
    /// some wait on a real wall-clock deadline (to exercise the
    /// kill/timeout path itself) — run enough of those at once on a
    /// loaded machine and they start starving each other for CPU and
    /// fork() scheduling, producing exactly the kind of flake a
    /// deadline bump can never fully outrun (observed in practice: a
    /// test that passes 5/5 in isolation failed 2/4 when run alongside
    /// this module's other subprocess tests). Acquiring this lock for
    /// the duration of a subprocess-spawning test serializes just the
    /// tests that actually need real OS scheduling, rather than paying
    /// for serialization (or chasing ever-larger deadlines) on the
    /// whole file.
    static SUBPROCESS_SERIAL: Mutex<()> = Mutex::new(());

    /// A single POSIX `sh` fixture script, mode-dispatched by `argv[1]`
    /// (not an env var — `env_clear` strips those before this process
    /// ever sees them). Uses only absolute paths, since `run_one_shot`
    /// clears `PATH`.
    fn fixture_script(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("fixture.sh");
        let script = r#"#!/bin/sh
mode="$1"
case "$mode" in
  ok)
    /bin/cat >/dev/null
    printf '%s' '{"ok":true}'
    exit 0
    ;;
  nonzero)
    /bin/cat >/dev/null
    printf '%s' '{"ok":true}'
    exit 7
    ;;
  garbage)
    /bin/cat >/dev/null
    printf '%s' 'not-json-at-all'
    exit 0
    ;;
  trailing)
    /bin/cat >/dev/null
    printf '%s' '{"ok":true}trailing-garbage'
    exit 0
    ;;
  oversized)
    /bin/cat >/dev/null
    /usr/bin/yes 'x' | /usr/bin/head -c "$2"
    exit 0
    ;;
  group_hold)
    /bin/sleep 30 &
    exec /bin/sleep 30
    ;;
  early_exit)
    /bin/sleep 30 &
    echo $! > "$2"
    exit 0
    ;;
  flood)
    /bin/cat >/dev/null
    exec /usr/bin/yes x
    ;;
  pwd)
    /bin/cat >/dev/null
    printf '{"cwd":"%s"}' "$(/bin/pwd)"
    exit 0
    ;;
  setsid_escape)
    /bin/cat >/dev/null
    exec /usr/bin/perl -e '
      use POSIX qw(setsid);
      my $pidfile = shift;
      my $pid = fork();
      if (!defined $pid) { exit 1; }
      if ($pid == 0) {
        POSIX::setsid();
        open(my $fh, ">", $pidfile) or exit 1;
        print $fh "$$\n";
        close $fh;
        sleep 30;
        exit 0;
      }
      exit 0;
    ' "$2"
    ;;
  *)
    exit 99
    ;;
esac
"#;
        fs::write(&path, script).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn run(args: &[&str], deadline_ms: u64) -> Result<Vec<u8>, RunError> {
        let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(dir.path());
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let cancel = AtomicBool::new(false);
        run_one_shot(
            script.to_str().unwrap(),
            &args,
            b"{}",
            Duration::from_millis(deadline_ms),
            &cancel,
        )
    }

    // These "should succeed quickly" tests use a generous deadline
    // (not a tight one) on purpose: the fixture scripts normally
    // complete in single-digit milliseconds, but this module's many
    // subprocess-spawning tests run in parallel by default within this
    // one test binary, and — observed in practice on this machine
    // under heavy external load (another worktree's `cargo doc` run
    // holding the shared build lock for hours) — that parallelism can
    // occasionally push even a trivial fork+exec well past a second.
    // The deadline here is "don't hang forever," not "must be fast" —
    // speed is asserted only in the tests that specifically exercise
    // the timeout/kill path below.
    const GENEROUS_DEADLINE_MS: u64 = 15_000;

    #[test]
    fn happy_path_returns_the_single_frame() {
        let frame = run(&["ok"], GENEROUS_DEADLINE_MS).unwrap();
        assert_eq!(frame, br#"{"ok":true}"#);
    }

    #[test]
    fn nonzero_exit_is_a_failure_even_with_a_valid_frame() {
        let error = run(&["nonzero"], GENEROUS_DEADLINE_MS).unwrap_err();
        assert!(matches!(error, RunError::NonZeroExit(Some(7))));
    }

    #[test]
    fn oversized_response_is_rejected() {
        let error = run(
            &["oversized", &(MAX_RESPONSE_BYTES + 1).to_string()],
            GENEROUS_DEADLINE_MS,
        )
        .unwrap_err();
        assert!(matches!(error, RunError::ResponseTooLarge));
    }

    // Regression test: a child that floods stdout indefinitely (never
    // hits EOF on its own, unlike the `oversized`-mode fixture which
    // writes an exact byte count and then exits) must still be caught
    // and killed promptly once the cap is exceeded — not left to burn
    // the whole deadline waiting for an exit that will never happen
    // while its writer thread sits blocked on a full pipe.
    #[test]
    fn flooding_child_is_capped_and_killed_promptly_not_timed_out() {
        let start = Instant::now();
        // A generous deadline (20s) so that, under heavy parallel-test
        // load, a genuinely prompt cap-detection+kill (expected to take
        // single-digit milliseconds) still has enormous headroom before
        // it would be indistinguishable from simply timing out.
        let error = run(&["flood"], 20_000).unwrap_err();
        assert!(matches!(error, RunError::ResponseTooLarge));
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "took {:?}, expected well under the 20s deadline if the cap triggered the kill \
             rather than the deadline itself",
            start.elapsed()
        );
    }

    #[test]
    fn exact_cap_sized_response_is_accepted_by_the_transport() {
        let frame = run(
            &["oversized", &MAX_RESPONSE_BYTES.to_string()],
            GENEROUS_DEADLINE_MS,
        )
        .unwrap();
        assert_eq!(frame.len(), MAX_RESPONSE_BYTES);
    }

    #[test]
    fn request_over_the_cap_is_rejected_before_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(dir.path());
        let cancel = AtomicBool::new(false);
        let oversized_request = vec![b'x'; MAX_REQUEST_BYTES + 1];
        let error = run_one_shot(
            script.to_str().unwrap(),
            &["ok".to_string()],
            &oversized_request,
            Duration::from_millis(GENEROUS_DEADLINE_MS),
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(error, RunError::RequestTooLarge(_)));
    }

    #[test]
    fn timeout_kills_the_whole_owned_process_group_within_the_deadline_plus_margin() {
        let start = Instant::now();
        // Deliberately a short deadline (this test exercises the
        // timeout path itself), but the elapsed-time assertion below is
        // generous for the same parallel-load reason as
        // `GENEROUS_DEADLINE_MS` above.
        let error = run(&["group_hold"], 500).unwrap_err();
        assert!(matches!(error, RunError::Timeout));
        // If only the direct child (not the whole group) had been
        // killed, the backgrounded `sleep 30` holding stdout open would
        // have made the reader thread block for the full 30s instead of
        // returning promptly after the deadline.
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "took {:?}, expected well under 15s if the whole group died",
            start.elapsed()
        );
    }

    // Regression test: a leader that backgrounds a grandchild holding
    // stdout open and then exits *immediately* (rather than lingering,
    // as `group_hold` does) must still have that grandchild killed
    // within the deadline. Phase 1 deliberately never calls `try_wait`
    // while waiting for a response frame, specifically so an
    // already-reaped-or-reapable leader can't cause this code to
    // declare victory while a live grandchild still holds the pipe.
    #[test]
    fn leader_exiting_immediately_does_not_leak_a_grandchild_holding_stdout() {
        let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(dir.path());
        let pidfile = dir.path().join("grandchild.pid");
        let args = vec![
            "early_exit".to_string(),
            pidfile.to_str().unwrap().to_string(),
        ];
        let cancel = AtomicBool::new(false);
        let start = Instant::now();
        let error = run_one_shot(
            script.to_str().unwrap(),
            &args,
            b"{}",
            // Generous relative to the assertion below: this module's
            // own tests spawn many subprocesses in parallel within one
            // test binary, and — observed in practice on this machine
            // under heavy external load — even a trivial fork+exec can
            // take several seconds to actually start running and write
            // its pidfile under that scheduling pressure, which would
            // otherwise make this test flaky for a reason unrelated to
            // what it verifies.
            Duration::from_millis(6_000),
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(error, RunError::Timeout));
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "took {:?}",
            start.elapsed()
        );

        // Confirm the grandchild is actually dead — not merely that we
        // stopped waiting on it. `kill(pid, 0)` can keep succeeding on
        // an already-killed-but-not-yet-reaped zombie until the OS
        // reaps it, so poll rather than sleeping once and risking a
        // flake.
        let pid_text = fs::read_to_string(&pidfile).unwrap();
        let grandchild_pid: i32 = pid_text.trim().parse().unwrap();
        use rustix::io::Errno;
        use rustix::process::{test_kill_process, Pid};
        let grandchild_pid = Pid::from_raw(grandchild_pid).expect("a valid pid was recorded");
        let poll_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if test_kill_process(grandchild_pid) == Err(Errno::SRCH) {
                break;
            }
            assert!(
                Instant::now() < poll_deadline,
                "grandchild pid {grandchild_pid:?} should have been killed via the owned process group"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn cancellation_flag_stops_a_hanging_child_promptly() {
        use std::sync::Arc;
        let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(dir.path());
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_setter = Arc::clone(&cancel);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            cancel_setter.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        let error = run_one_shot(
            script.to_str().unwrap(),
            &["group_hold".to_string()],
            b"{}",
            Duration::from_secs(30),
            &cancel,
        )
        .unwrap_err();
        handle.join().unwrap();
        assert!(matches!(error, RunError::Cancelled));
        assert!(start.elapsed() < Duration::from_secs(15));
    }

    // Regression test for B1 (HORO-1854 adversarial review): a
    // grandchild that calls `setsid()` leaves the owned process group
    // entirely, so `kill_owned_process_group` can never reach it — it
    // keeps the stdout pipe's write end open forever, and the reader
    // thread blocks in `read_to_end` with no EOF ever arriving. Before
    // this fix, `run_one_shot`'s tail unconditionally called
    // `reader.join()`, which then blocked for as long as the escaped
    // process lived (confirmed by probe: ~12.4s against a 1s deadline).
    // After the fix, the thread is joined only when its channel recv
    // already proved it was done, so this must return `Timeout` close
    // to the deadline, not 10x it.
    #[test]
    fn escaped_setsid_grandchild_cannot_block_the_return_past_deadline_plus_grace() {
        let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(dir.path());
        let pidfile = dir.path().join("escaped.pid");
        let args = vec![
            "setsid_escape".to_string(),
            pidfile.to_str().unwrap().to_string(),
        ];
        let cancel = AtomicBool::new(false);
        let deadline_ms = 1_000;
        let start = Instant::now();
        let error = run_one_shot(
            script.to_str().unwrap(),
            &args,
            b"{}",
            Duration::from_millis(deadline_ms),
            &cancel,
        )
        .unwrap_err();
        let elapsed = start.elapsed();
        assert!(matches!(error, RunError::Timeout));
        // Bound is `deadline + 2 * JOIN_GRACE` (see the module docs);
        // generous headroom on top for scheduling jitter under parallel
        // test load, but nowhere near the 30s the escaped process sleeps
        // for — that gap is exactly what distinguishes "bounded" from
        // "blocked on the attacker".
        assert!(
            elapsed < Duration::from_secs(10),
            "took {elapsed:?}, expected well under 10s if the escaped grandchild \
             could not block the return"
        );

        // Clean up the escaped process so the test run doesn't leak it
        // (it survives the group kill by design — that's the point of
        // this test). Poll for the pidfile briefly: the grandchild write
        // can lag slightly under scheduling pressure.
        let poll_deadline = Instant::now() + Duration::from_secs(5);
        let mut grandchild_pid: Option<i32> = None;
        while Instant::now() < poll_deadline {
            if let Ok(text) = fs::read_to_string(&pidfile) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    grandchild_pid = Some(pid);
                    break;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        if let Some(pid) = grandchild_pid {
            use rustix::process::{Pid, Signal};
            if let Some(pid) = Pid::from_raw(pid) {
                let _ = rustix::process::kill_process(pid, Signal::KILL);
            }
        }
    }

    #[test]
    fn child_runs_with_a_fixed_working_directory_regardless_of_the_hosts_own_cwd() {
        let frame = run(&["pwd"], GENEROUS_DEADLINE_MS).unwrap();
        assert_eq!(frame, br#"{"cwd":"/"}"#);
    }

    fn minimal_approved_registration(executable: &std::path::Path) -> Registration {
        use crate::registration::{
            Approval, Consent, ExpectedConstraint, FixedEnvironment, ObservationConfidence,
            RegisteredContext, RuntimeFileDigest, Transport, TransportKind, TrustMode,
        };
        use crate::wire::{Metric, Scope, Unit};
        let mut registration = Registration {
            registration_id: "reg-a".into(),
            extension_id: "ext.example".into(),
            extension_version: "1.0.0".into(),
            enabled: true,
            transport: Transport {
                kind: TransportKind::OneshotStdio,
                executable: executable.to_str().unwrap().to_owned(),
                args: vec!["ok".into()],
                environment: FixedEnvironment {
                    lang: "C.UTF-8".into(),
                    lc_all: "C.UTF-8".into(),
                },
            },
            context: RegisteredContext {
                selector: "sel".into(),
                handle: "handle".into(),
                principal: "principal".into(),
                authority: "authority".into(),
            },
            expected: vec![ExpectedConstraint {
                id: "c1".into(),
                scope: Scope::Account {
                    handle: "acct".into(),
                },
                metric: Metric::TotalTokens,
                unit: Unit::Tokens,
                window_uuid: "win-1".into(),
                provider: "anthropic".into(),
                account_fingerprint: "fp".into(),
                pool_alias: "alias".into(),
                max_staleness_secs: 60,
                window_length_secs: 3600,
                capacity_ceiling: None,
            }],
            binding_revision: "1".into(),
            binding_verified_at: "2026-10-12T00:00:00Z".into(),
            scope_configured_at: "2026-10-12T00:00:00Z".into(),
            timeout_ms: 10_000,
            trust_mode: TrustMode::ObservationOnly,
            observation_confidence: ObservationConfidence::Low,
            approval: Approval {
                approved: true,
                revision: "1".into(),
                executable_sha256: String::new(),
                manifest_sha256: String::new(),
                launch_tuple_sha256: String::new(),
                runtime_files: Vec::<RuntimeFileDigest>::new(),
                endpoint_owner: "owner".into(),
                consent: Consent {
                    consented_by: "operator".into(),
                    recorded_at_reference: "ref-1".into(),
                },
            },
        };
        // Approve: fill in the three bound digests from current disk
        // state, exactly as an approval workflow would, reusing
        // `identity`'s own digest functions (now `pub(crate)`) rather
        // than re-deriving the same canonical-JSON math here — that
        // duplication previously skipped `identity`'s defensive
        // `sort_all_objects()` call and would have silently diverged
        // under Cargo feature unification (e.g. some other workspace
        // member enabling `serde_json/preserve_order`).
        let executable_digest = identity::digest_file(executable).unwrap();
        registration.approval.executable_sha256 = executable_digest.clone();
        registration.approval.manifest_sha256 = identity::manifest_digest(&registration);
        registration.approval.launch_tuple_sha256 =
            identity::launch_tuple_digest(&registration, &executable_digest, &[]);
        registration
    }

    #[test]
    fn invoke_runs_the_transport_after_registration_and_identity_checks_pass() {
        let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(dir.path());
        let registration = minimal_approved_registration(&script);
        let cancel = AtomicBool::new(false);
        let frame = invoke(&registration, b"{}", &cancel).unwrap();
        assert_eq!(frame, br#"{"ok":true}"#);
    }

    #[test]
    fn invoke_refuses_a_disabled_registration_before_ever_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(dir.path());
        let mut registration = minimal_approved_registration(&script);
        registration.enabled = false;
        let cancel = AtomicBool::new(false);
        let error = invoke(&registration, b"{}", &cancel).unwrap_err();
        assert!(matches!(
            error,
            InvokeError::Identity(IdentityError::Disabled)
        ));
    }

    #[test]
    fn invoke_refuses_when_the_executable_was_edited_after_approval() {
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(dir.path());
        let registration = minimal_approved_registration(&script);
        // Re-approve, then tamper with the file afterward.
        fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        let cancel = AtomicBool::new(false);
        let error = invoke(&registration, b"{}", &cancel).unwrap_err();
        assert!(matches!(
            error,
            InvokeError::Identity(IdentityError::ExecutableDigestMismatch { .. })
        ));
    }

    #[test]
    fn frame_parsing_accepts_exactly_one_value_with_optional_trailing_newline() {
        let value: serde_json::Value = parse_single_frame(br#"{"a":1}"#).unwrap();
        assert_eq!(value, serde_json::json!({"a":1}));
        let value: serde_json::Value = parse_single_frame(b"{\"a\":1}\n").unwrap();
        assert_eq!(value, serde_json::json!({"a":1}));
    }

    #[test]
    fn frame_parsing_rejects_trailing_garbage_and_empty_and_invalid() {
        assert!(matches!(
            parse_single_frame::<serde_json::Value>(b"").unwrap_err(),
            FrameError::Empty
        ));
        assert!(matches!(
            parse_single_frame::<serde_json::Value>(b"not-json").unwrap_err(),
            FrameError::Invalid(_)
        ));
        assert!(matches!(
            parse_single_frame::<serde_json::Value>(br#"{"a":1}{"b":2}"#).unwrap_err(),
            FrameError::TrailingOutput
        ));
        assert!(matches!(
            parse_single_frame::<serde_json::Value>(br#"{"a":1}garbage"#).unwrap_err(),
            FrameError::TrailingOutput
        ));
    }

    #[test]
    fn runner_surfaces_garbage_and_trailing_output_as_raw_frames_for_the_caller_to_parse() {
        // run_one_shot itself is transport-only; framing validation is a
        // separate call the caller makes on the returned bytes.
        let frame = run(&["garbage"], GENEROUS_DEADLINE_MS).unwrap();
        assert!(parse_single_frame::<serde_json::Value>(&frame).is_err());
        let frame = run(&["trailing"], GENEROUS_DEADLINE_MS).unwrap();
        assert!(parse_single_frame::<serde_json::Value>(&frame).is_err());
    }
}
