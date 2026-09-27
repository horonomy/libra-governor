//! End-to-end tests of the real `libra-governor` binary's `doctor`,
//! `install`, and `uninstall` subcommands (HORO-1150). Mirrors
//! `hook_cli_integration.rs`'s pattern: spawn the real built binary as a
//! subprocess, drive it with real env vars and a real temp state dir /
//! Claude settings dir, assert on its real stdout/stderr/exit code.
//!
//! # What this file is the automated evidence for
//!
//! - A never-installed machine: `doctor` reports "not installed" and
//!   exits `0` — and, just as importantly, never creates `ledger.sqlite3`
//!   or spawns a daemon merely by being asked to diagnose.
//! - The full `install` -> `doctor` (healthy) -> `uninstall --yes` ->
//!   `doctor` (not installed again) lifecycle the ticket asks for.
//! - `doctor` correctly flags a deliberately corrupt `config.json` and a
//!   missing daemon as unhealthy/absent with a real diagnostic message,
//!   not a fabricated one.
//! - The secret-safety guarantee: a fake credential reference placed in
//!   `config.json` never appears in `doctor`'s stdout or stderr, in
//!   either human or `--json` form — a stronger guarantee than reasoning
//!   about which fields exist, because it survives future field
//!   additions to `DoctorResult`.

use std::process::{Child, Command, Stdio};
use std::sync::{Once, RwLock, RwLockReadGuard};
use std::time::Duration;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

/// Serializes "this process holds a writable descriptor to a binary this
/// file is about to exec" against "this process forks" (HORO-1500).
///
/// The kernel refuses `execve` on any file that *some* process currently
/// holds open for writing, with `ETXTBSY` /
/// `std::io::ErrorKind::ExecutableFileBusy`. `libtest` runs this file's
/// tests on several threads of one process, and every [`Sandbox`] writes
/// its own private copy of the binary, so without this lock the following
/// interleaving is reachable:
///
/// 1. Thread A is inside `fs::copy`, so the process holds a writable
///    descriptor to A's `bin_path`.
/// 2. Thread B, running a different test, calls `Command::spawn`. On Linux
///    that is `posix_spawn`, which glibc implements as
///    `clone(CLONE_VM | CLONE_VFORK)` followed by `execve` *in the child*;
///    the child starts out holding a duplicate of every descriptor this
///    process has open, including A's.
/// 3. `O_CLOEXEC` does not close that window: it only takes effect at the
///    child's own `execve`, and until then the descriptor is genuinely
///    open for writing.
/// 4. Thread A execs its own `bin_path`, the kernel sees an outstanding
///    writable descriptor to it, and A fails with `ETXTBSY` — in a thread
///    that did nothing wrong and for a reason unrelated to the code under
///    test.
///
/// Holding the exclusive side for exactly as long as a writable descriptor
/// to a binary copy exists, and the shared side across the fork/exec
/// transition, makes that interleaving unreachable rather than merely
/// unlikely — so this file needs no retry-on-`ETXTBSY` and no sleep.
/// Descriptor tables are per-process, which is why an in-process lock is a
/// complete answer: another process copying another file can never make
/// this process's exec fail.
static BINARY_COPY_VS_FORK: RwLock<()> = RwLock::new(());

/// How many [`copy_binary_for_exec`] calls currently hold a writable
/// descriptor to a binary copy open. Incremented and decremented only
/// under the exclusive side of [`BINARY_COPY_VS_FORK`], so a holder of the
/// shared side must observe zero.
///
/// This turns the invariant [`BINARY_COPY_VS_FORK`] exists to provide into
/// something every fork in this file checks directly, which matters because
/// the kernels this suite runs on do not agree about `ETXTBSY`: Linux
/// enforces it, while macOS's `posix_spawn` is a single kernel call with no
/// intermediate child to inherit a descriptor at all, so a regression here
/// is invisible on a developer's Mac and reappears only in CI. Asserting on
/// the *cause* rather than waiting for one platform's symptom reports the
/// regression by name, on every platform, the first time the interleaving
/// occurs.
static BINARY_COPIES_IN_FLIGHT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Acquires the shared side of [`BINARY_COPY_VS_FORK`] for the duration of
/// a fork. Every `Command` in this file goes through a holder of this
/// guard, including the ones whose own exec target is never written to: it
/// is the *forking* that hands a sibling thread's writable descriptor to a
/// child, so every fork has to be excluded from a copy, not just the ones
/// that exec a freshly copied binary.
fn fork_guard() -> RwLockReadGuard<'static, ()> {
    BINARY_COPY_VS_FORK
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Asserts the invariant [`fork_guard`] exists to buy, deliberately as a
/// statement of its own rather than as part of acquiring the guard: the two
/// then fail independently, so deleting the guard is caught by this
/// assertion instead of silently reintroducing a race that only some
/// kernels report and only sometimes.
///
/// Skipped while this thread is already panicking: a fork can happen during
/// unwinding (fixture teardown), and a second panic there would abort the
/// process instead of reporting anything.
fn assert_no_binary_copy_in_flight() {
    if std::thread::panicking() {
        return;
    }
    assert_eq!(
        BINARY_COPIES_IN_FLIGHT.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "this process forked while a binary copy held a writable descriptor open: the \
         copy-versus-fork serialization documented on BINARY_COPY_VS_FORK has regressed \
         (HORO-1500), and this fork can make an unrelated thread's exec fail with ETXTBSY"
    );
}

/// Copies `src` over `dst` and makes `dst` executable, with no fork of this
/// process able to observe the writable descriptor `fs::copy` opens. See
/// [`BINARY_COPY_VS_FORK`].
fn copy_binary_for_exec(src: &std::path::Path, dst: &std::path::Path) {
    let _exclusive = BINARY_COPY_VS_FORK
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // `fs::copy` closes both handles before it returns, so the counter
    // brackets exactly the window in which a writable descriptor exists.
    BINARY_COPIES_IN_FLIGHT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let copied = std::fs::copy(src, dst);
    BINARY_COPIES_IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    copied.unwrap_or_else(|e| panic!("copying {} to {} failed: {e}", src.display(), dst.display()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Spawns `cmd` while holding [`fork_guard`], returning the error rather
/// than panicking — for the best-effort execs (warm-ups, daemon teardown)
/// whose failure must never fail a test.
///
/// The guard is released as soon as `spawn` returns, which is after the
/// descriptor-inheriting window has closed: both glibc's `posix_spawn` and
/// std's fork+exec fallback report the child's *exec* failure through the
/// parent's return value, so a returned `Ok` means the child has already
/// reached `execve` and dropped its inherited `O_CLOEXEC` descriptors. The
/// child is never waited on under the guard, so tests still run
/// concurrently.
fn try_spawn_binary(cmd: &mut Command) -> std::io::Result<Child> {
    let _guard = fork_guard();
    assert_no_binary_copy_in_flight();
    cmd.spawn()
}

/// [`try_spawn_binary`] for the execs a test depends on, with a failure
/// message that names this file's invariant so a future regression is not
/// mistaken for a flaky test.
fn spawn_binary(cmd: &mut Command) -> Child {
    try_spawn_binary(cmd).unwrap_or_else(|e| {
        panic!(
            "spawning {:?} failed: {e} (kind {:?}). An ExecutableFileBusy here means the \
             copy-versus-fork serialization documented on BINARY_COPY_VS_FORK has regressed \
             (HORO-1500), not that the binary is wrong",
            cmd.get_program(),
            e.kind()
        )
    })
}

/// Same one-time warm-up rationale as `hook_cli_integration.rs`.
fn warm_up_binary() {
    static WARM_UP: Once = Once::new();
    WARM_UP.call_once(|| {
        let mut cmd = Command::new(bin());
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut child) = try_spawn_binary(&mut cmd) {
            let _ = child.wait();
        }
    });
}

struct Sandbox {
    _state_parent: tempfile::TempDir,
    _claude_parent: tempfile::TempDir,
    _bin_dir: tempfile::TempDir,
    state_dir: std::path::PathBuf,
    claude_dir: std::path::PathBuf,
    /// A private copy of the shared `CARGO_BIN_EXE_libra-governor`
    /// binary. `uninstall --yes` deletes the exact binary path its
    /// install marker recorded once it matches `current_exe()` — see
    /// `uninstall_cmd`'s module docs — and every test in this file
    /// shares one on-disk `CARGO_BIN_EXE_libra-governor` binary. Without
    /// a private copy, one test's `uninstall --yes` would delete the
    /// binary every other test in this same test run also needs to
    /// exec, an entirely different hazard from the real-world behavior
    /// this file is meant to exercise.
    bin_path: std::path::PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        warm_up_binary();
        let state_parent = tempfile::tempdir().unwrap();
        let claude_parent = tempfile::tempdir().unwrap();
        let bin_dir = tempfile::tempdir().unwrap();
        let state_dir = state_parent.path().join("state");
        let claude_dir = claude_parent.path().join("claude");
        std::fs::create_dir_all(&claude_dir).unwrap();

        let bin_path = bin_dir.path().join("libra-governor");
        copy_binary_for_exec(std::path::Path::new(bin()), &bin_path);
        // Each private copy is a distinct file on disk and pays its own
        // first-execution OS validation cost (macOS Gatekeeper/AMFI) the
        // same way the original binary does — see
        // `hook_cli_integration.rs`'s `warm_up_binary` docs. Absorb it
        // here, untimed, so a test's own timing assertion only measures
        // this tool's own logic.
        let mut warm_up = Command::new(&bin_path);
        warm_up
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut child) = try_spawn_binary(&mut warm_up) {
            let _ = child.wait();
        }

        Sandbox {
            _state_parent: state_parent,
            _claude_parent: claude_parent,
            _bin_dir: bin_dir,
            state_dir,
            claude_dir,
            bin_path,
        }
    }

    fn run(&self, args: &[&str]) -> (std::process::Output, Duration) {
        let mut cmd = Command::new(&self.bin_path);
        cmd.args(args)
            .env("LIBRA_GOVERNOR_STATE_DIR", &self.state_dir)
            .env("LIBRA_GOVERNOR_CLAUDE_DIR", &self.claude_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let start = std::time::Instant::now();
        let output = spawn_binary(&mut cmd).wait_with_output().unwrap();
        (output, start.elapsed())
    }
}

fn kill_daemon_for(state_dir: &std::path::Path) {
    let mut cmd = Command::new("pkill");
    cmd.args(["-f", &state_dir.display().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(mut child) = try_spawn_binary(&mut cmd) {
        let _ = child.wait();
    }
}

#[test]
fn doctor_on_a_never_installed_machine_exits_zero_and_creates_nothing() {
    let sandbox = Sandbox::new();
    let (output, elapsed) = sandbox.run(&["doctor"]);

    assert!(
        output.status.success(),
        "doctor on a fresh machine must exit 0 (nothing is broken, just not installed); \
         stderr/stdout: {} {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "doctor must never spawn a daemon or hang"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.to_lowercase().contains("not installed"));

    assert!(
        !sandbox.state_dir.exists(),
        "a read-only diagnostic must not create the state dir"
    );
}

#[test]
fn install_doctor_uninstall_doctor_lifecycle() {
    let sandbox = Sandbox::new();

    let (install_output, _) = sandbox.run(&["install"]);
    assert!(
        install_output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&install_output.stderr)
    );

    let (doctor_output, _) = sandbox.run(&["doctor"]);
    assert!(doctor_output.status.success());
    let stdout = String::from_utf8_lossy(&doctor_output.stdout);
    assert!(
        stdout.contains("hooks and statusline wired"),
        "doctor after install must report hooks wired: {stdout}"
    );

    // uninstall never deletes the daemon binary itself (it only reports
    // how to remove it via `cargo uninstall`, to avoid corrupting
    // cargo's own package bookkeeping — see uninstall_cmd's module
    // docs), so sandbox.bin_path stays usable for the post-uninstall
    // doctor check below with no copy/workaround needed.
    //
    // uninstall's confirmation prompt reads stdin; Stdio::null() is not a
    // TTY, so the state dir deletion is safely skipped without --yes.
    // Passing --yes here exercises the confirmed deletion path.
    let (uninstall_output, _) = sandbox.run(&["uninstall", "--yes"]);
    assert!(
        uninstall_output.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&uninstall_output.stderr)
    );
    assert!(
        sandbox.bin_path.exists(),
        "uninstall must never delete the daemon binary directly"
    );

    let (doctor_again, _) = sandbox.run(&["doctor"]);
    assert!(doctor_again.status.success());
    let stdout_again = String::from_utf8_lossy(&doctor_again.stdout);
    assert!(
        stdout_again.to_lowercase().contains("not installed"),
        "doctor after uninstall must report not-installed again: {stdout_again}"
    );

    kill_daemon_for(&sandbox.state_dir);
}

#[test]
fn uninstall_without_yes_on_a_non_interactive_stdin_never_deletes_the_state_dir() {
    let sandbox = Sandbox::new();
    sandbox.run(&["install"]);
    // Force the state dir to exist even without a daemon having run.
    std::fs::create_dir_all(&sandbox.state_dir).unwrap();
    std::fs::write(sandbox.state_dir.join("ledger.sqlite3"), b"pretend-ledger").unwrap();

    let (output, _) = sandbox.run(&["uninstall"]);
    assert!(output.status.success());
    assert!(
        sandbox.state_dir.join("ledger.sqlite3").exists(),
        "a non-interactive uninstall without --yes must never delete real ledger data"
    );

    kill_daemon_for(&sandbox.state_dir);
}

#[test]
fn doctor_flags_a_corrupt_config_json_with_a_live_daemon_as_unhealthy() {
    let sandbox = Sandbox::new();

    // Get a real daemon running via the hook path (mirrors
    // hook_cli_integration.rs), *before* writing the corrupt config —
    // handle_doctor re-reads config.json fresh on every request (the
    // same re-validate-rather-than-cache pattern GatewayStatus already
    // uses), so a daemon that started clean still reports it once the
    // file is written afterward.
    let payload = serde_json::json!({
        "session_id": "doctor-corrupt-config",
        "cwd": sandbox.state_dir.parent().unwrap(),
        "prompt": "anything",
    })
    .to_string();
    warm_up_binary();
    let mut hook = Command::new(&sandbox.bin_path);
    hook.args(["hook", "user-prompt-submit"])
        .env("LIBRA_GOVERNOR_STATE_DIR", &sandbox.state_dir)
        .env("LIBRA_GOVERNOR_CLAUDE_DIR", &sandbox.claude_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = spawn_binary(&mut hook);
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), payload.as_bytes()).unwrap();
    child.wait().unwrap();

    std::fs::write(sandbox.state_dir.join("config.json"), "{ not json").unwrap();

    let (output, _) = sandbox.run(&["doctor", "--json"]);
    assert!(
        !output.status.success(),
        "a rejected config.json must be reported as an Error-severity finding, non-zero exit"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("doctor --json did not print valid JSON: {e}\n{stdout}"));
    let findings = value["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f["id"] == "config_file_valid" && f["severity"] == "error"),
        "expected a config_file_valid error finding: {findings:#?}"
    );

    kill_daemon_for(&sandbox.state_dir);
}

#[test]
fn doctor_with_no_daemon_and_no_socket_still_exits_zero_and_stays_fast() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(&sandbox.state_dir).unwrap();

    let (output, elapsed) = sandbox.run(&["doctor"]);
    assert!(output.status.success());
    assert!(
        elapsed < Duration::from_secs(5),
        "connect-only must fail fast when nothing is listening, doctor took {elapsed:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("daemon is not running"));
}

/// The secret-safety guarantee: a fake credential *reference* (never a
/// real secret — see `SECURITY.md`) placed in `config.json`'s
/// `credential_command`/`credential_args` must never appear in `doctor`'s
/// stdout or stderr, in either human or `--json` form. Stronger than
/// reasoning about which `DoctorResult` fields exist, because it survives
/// future field additions.
#[test]
fn doctor_never_prints_a_gateway_credential_reference() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(&sandbox.state_dir).unwrap();
    let needle = "op://vault/NEEDLE-a1b2c3d4";
    let config = serde_json::json!({
        "gateway": {
            "bind_addr": "127.0.0.1:0",
            "token_path": sandbox.state_dir.join("gateway.token"),
            "credential_mode": "governor_held",
            "credential_command": "op",
            "credential_args": ["read", needle]
        }
    });
    std::fs::write(
        sandbox.state_dir.join("config.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();

    // Start a real daemon against this exact config.json (loaded fresh
    // by `handle_doctor` on every request), so this test exercises the
    // full gateway-configured reporting path — capabilities,
    // gateway_credential_configured, everything — not just the
    // no-daemon local-only findings.
    let payload = serde_json::json!({
        "session_id": "doctor-credential-needle",
        "cwd": sandbox.state_dir.parent().unwrap(),
        "prompt": "anything",
    })
    .to_string();
    warm_up_binary();
    let mut hook = Command::new(&sandbox.bin_path);
    hook.args(["hook", "user-prompt-submit"])
        .env("LIBRA_GOVERNOR_STATE_DIR", &sandbox.state_dir)
        .env("LIBRA_GOVERNOR_CLAUDE_DIR", &sandbox.claude_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = spawn_binary(&mut hook);
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), payload.as_bytes()).unwrap();
    child.wait().unwrap();

    for args in [vec!["doctor"], vec!["doctor", "--json"]] {
        let (output, _) = sandbox.run(&args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stdout.contains(needle) && !stderr.contains(needle),
            "doctor {args:?} leaked the credential reference:\nstdout: {stdout}\nstderr: {stderr}"
        );
    }

    kill_daemon_for(&sandbox.state_dir);
}

/// Regression coverage for HORO-1500: several threads creating sandboxes
/// and exec'ing their own private binary copy at the same time must never
/// fail to exec, and must never fork while a copy is in flight.
///
/// Both halves of the invariant are checked, and they fail for different
/// reasons on purpose. `fork_guard`'s in-flight assertion fires on any
/// overlap at all, which is deterministic and platform-independent;
/// `spawn_binary`'s `ExecutableFileBusy` diagnostic fires only on the much
/// narrower kernel window, and only on kernels that enforce it. Removing
/// the serialization therefore fails this test loudly on macOS as well as
/// on Linux, which is where the original defect was only ever observed.
///
/// The thread count matches the file's own test count, so the interleaving
/// exercised here is the one `libtest` itself produces when it runs this
/// file. The round count is deliberately small, and measured rather than
/// guessed: with these values, reverting either half of the fix (the
/// exclusive side of the copy, or the fork guard) was caught in 20 of 20
/// runs on Linux, and in 10 of 10 runs pinned to two CPUs. Every sandbox
/// runs `doctor`, the cheapest subcommand that both execs the copy and
/// exits zero without spawning a daemon.
#[test]
fn concurrent_sandboxes_always_exec_their_own_private_binary_copy() {
    const THREADS: usize = 6;
    const ROUNDS: usize = 4;

    let start_together = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
    let workers: Vec<_> = (0..THREADS)
        .map(|thread| {
            let start_together = std::sync::Arc::clone(&start_together);
            std::thread::spawn(move || {
                start_together.wait();
                for round in 0..ROUNDS {
                    let sandbox = Sandbox::new();
                    let (output, _) = sandbox.run(&["doctor"]);
                    assert!(
                        output.status.success(),
                        "doctor exited {:?} on thread {thread} round {round}; stderr: {}",
                        output.status.code(),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
            })
        })
        .collect();

    let mut completed = 0usize;
    for (thread, worker) in workers.into_iter().enumerate() {
        match worker.join() {
            Ok(()) => completed += 1,
            Err(_) => panic!(
                "worker thread {thread} panicked; its panic message above names the cause. \
                 An ExecutableFileBusy or an in-flight-copy assertion there means the \
                 copy-versus-fork serialization documented on BINARY_COPY_VS_FORK has \
                 regressed (HORO-1500)"
            ),
        }
    }
    assert_eq!(
        completed, THREADS,
        "every worker thread must have finished all {ROUNDS} rounds"
    );
}
