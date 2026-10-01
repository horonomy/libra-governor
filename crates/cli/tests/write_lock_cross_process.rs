//! Cross-process evidence for ADR-0014's write lock (HORO-1380 S4c).
//!
//! These tests drive the real compiled `libra-governor` binary as two
//! genuinely separate OS processes (never threads in this test process
//! itself) via its hidden `__lock_test_hold <config-path> <hold-ms>`
//! subcommand (see `crates/cli/src/main.rs`'s dispatch arm and
//! `crates/cli/src/write_lock.rs::test_hold_cmd`), which does nothing
//! but call the same `write_lock::acquire` that `claude_settings`/
//! `codex_hooks_file` call. A same-process, same-descriptor test would
//! not prove cross-process exclusion — file descriptors from one process
//! can behave differently than two independent processes' descriptors —
//! so every test here spawns `std::process::Command`.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

/// Spawns the hidden lock-holder subcommand against `config_path`,
/// holding the lock for `hold_ms` milliseconds, and blocks (on a
/// blocking stdout read, never a sleep) until it prints "LOCKED" —
/// i.e. until it has genuinely acquired the lock, not merely started.
fn spawn_holder(config_path: &std::path::Path, hold_ms: u64) -> std::process::Child {
    let mut child = Command::new(bin())
        .args([
            "__lock_test_hold",
            config_path.to_str().unwrap(),
            &hold_ms.to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn lock-holder process");

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("failed to read holder's first line");
    assert_eq!(
        line.trim(),
        "LOCKED",
        "holder process must report successful acquisition before the test proceeds"
    );
    // The holder writes exactly one line and then never touches stdout
    // again, so dropping `reader` (closing our end of the pipe) here is
    // safe and does not risk a SIGPIPE on a later write.
    drop(reader);
    child
}

/// Test 1 — cross-process contention: a second process's acquisition is
/// genuinely blocked/retried while the first holds the lock, and
/// succeeds once the first releases it.
#[test]
fn second_process_blocks_and_then_succeeds_once_the_first_releases() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("settings.json");
    std::fs::write(&config_path, "{}").unwrap();

    // Holder 1 takes the lock for 800ms.
    let mut holder = spawn_holder(&config_path, 800);

    // Holder 2 attempts to acquire the same lock right away. It must
    // not acquire instantly — proving it genuinely retried against a
    // real contended lock rather than the first holder having already
    // finished.
    let start = Instant::now();
    let waiter = Command::new(bin())
        .args(["__lock_test_hold", config_path.to_str().unwrap(), "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = waiter.wait_with_output().unwrap();
    let elapsed = start.elapsed();

    assert!(
        output.status.success(),
        "the waiter must eventually succeed once holder 1 releases: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "LOCKED");
    assert!(
        elapsed >= Duration::from_millis(500),
        "the waiter must have genuinely blocked/retried against holder 1's lock, not acquired \
         instantly (elapsed = {elapsed:?})"
    );

    holder.wait().unwrap();
}

/// Test 2 — timeout/failure behavior: a holder keeps the lock beyond the
/// 5000ms budget; the waiter's acquisition times out with the distinct
/// typed error, and the config file's bytes are provably unchanged
/// (byte-for-byte) afterward.
#[test]
fn second_process_times_out_after_the_5000ms_budget_and_config_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("settings.json");
    let original_bytes = b"{\"untouched\": true}".to_vec();
    std::fs::write(&config_path, &original_bytes).unwrap();

    // Holder keeps the lock for 7000ms — longer than the 5000ms budget.
    let mut holder = spawn_holder(&config_path, 7000);

    let start = Instant::now();
    let output = Command::new(bin())
        .args(["__lock_test_hold", config_path.to_str().unwrap(), "0"])
        .output()
        .unwrap();
    let elapsed = start.elapsed();

    assert!(
        !output.status.success(),
        "acquisition must fail once the budget is exhausted"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("LOCK_FAILED:"),
        "must report the distinct lock-failure path, got: {stdout}"
    );
    assert!(
        stdout.contains("timed out") || stdout.contains("another Horonom writer"),
        "must report the contention/timeout message, got: {stdout}"
    );
    assert!(
        elapsed >= Duration::from_millis(4900) && elapsed < Duration::from_millis(6500),
        "must time out at approximately the 5000ms budget, not sooner or much later \
         (elapsed = {elapsed:?})"
    );

    let bytes_after = std::fs::read(&config_path).unwrap();
    assert_eq!(
        bytes_after, original_bytes,
        "a timed-out lock acquisition must leave the config file byte-for-byte unchanged"
    );

    holder.kill().ok();
    holder.wait().ok();
}

/// Test 3 — process death releases the lock: a holder is killed with
/// SIGKILL (not a clean exit), and a second, already-waiting process
/// successfully acquires the lock immediately after, with no stale-lock
/// cleanup step of any kind.
#[test]
fn killing_the_holder_releases_the_lock_for_the_next_waiter() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("settings.json");
    std::fs::write(&config_path, "{}").unwrap();

    // Holder would hold for 60s if left alone — long enough that only a
    // kill (not a timeout) could free the lock within this test's
    // window.
    let mut holder = spawn_holder(&config_path, 60_000);

    // SIGKILL — the kernel must release the flock(2) lock on process
    // death, per ADR-0014 section 9, with no recovery procedure needed.
    holder.kill().expect("failed to SIGKILL the holder");
    holder.wait().expect("failed to reap the killed holder");

    let start = Instant::now();
    let output = Command::new(bin())
        .args(["__lock_test_hold", config_path.to_str().unwrap(), "0"])
        .output()
        .unwrap();
    let elapsed = start.elapsed();

    assert!(
        output.status.success(),
        "the next waiter must acquire the lock immediately after the holder is killed: \
         stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "LOCKED");
    assert!(
        elapsed < Duration::from_secs(2),
        "must acquire quickly — no stale-lock timeout/backoff should be needed after a kill \
         (elapsed = {elapsed:?})"
    );
}

/// Test 4 — a failed lock acquisition (timeout) writes zero bytes: not
/// just an unchanged config file, but no temp file, no backup file, and
/// no partial write anywhere in the directory.
#[test]
fn a_timed_out_acquisition_leaves_no_artifacts_in_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("settings.json");
    std::fs::write(&config_path, "{}").unwrap();

    let mut holder = spawn_holder(&config_path, 7000);

    let output = Command::new(bin())
        .args(["__lock_test_hold", config_path.to_str().unwrap(), "0"])
        .output()
        .unwrap();
    assert!(!output.status.success());

    // Exactly two entries are permitted: the untouched config file and
    // the lock sidecar the holder created when it opened the lock file
    // (ADR-0014: the sidecar itself, zero bytes, is not an artifact of
    // the write — see section 3, "the only filesystem effects permitted
    // before the lock is held"). Nothing else — no temp file, no
    // backup — may exist.
    let mut entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec![
            "settings.json".to_string(),
            "settings.json.horonom-write.lock".to_string(),
        ],
        "a timed-out acquisition must leave no temp file, no backup, and no partial write"
    );

    let sidecar_bytes = std::fs::read(dir.path().join("settings.json.horonom-write.lock")).unwrap();
    assert!(
        sidecar_bytes.is_empty(),
        "the sidecar must remain zero bytes — it is never written to"
    );

    holder.kill().ok();
    holder.wait().ok();
}

/// Test 5 — unrelated config files do not contend: locking one config's
/// sidecar must never block or interact with a different config's
/// sidecar, even in the same directory.
#[test]
fn unrelated_config_files_do_not_contend() {
    let dir = tempfile::tempdir().unwrap();
    let settings_path = dir.path().join("settings.json");
    let hooks_path = dir.path().join("hooks.json");
    std::fs::write(&settings_path, "{}").unwrap();
    std::fs::write(&hooks_path, "{}").unwrap();

    // Hold settings.json's lock for a long time.
    let mut holder = spawn_holder(&settings_path, 3000);

    // hooks.json's lock must be acquirable immediately, independent of
    // settings.json's contention.
    let start = Instant::now();
    let output = Command::new(bin())
        .args(["__lock_test_hold", hooks_path.to_str().unwrap(), "0"])
        .output()
        .unwrap();
    let elapsed = start.elapsed();

    assert!(
        output.status.success(),
        "a different config file's lock must never be blocked by an unrelated config's lock"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "LOCKED");
    assert!(
        elapsed < Duration::from_millis(500),
        "must acquire instantly — no contention with the unrelated lock (elapsed = {elapsed:?})"
    );

    holder.kill().ok();
    holder.wait().ok();
}
