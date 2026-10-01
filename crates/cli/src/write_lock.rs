//! Cross-process host-wide one-writer lock (ADR-0014, HORO-1380 slice
//! S4c).
//!
//! Shared by [`crate::claude_settings`] and [`crate::codex_hooks_file`]
//! so both config writers use byte-for-byte identical lock-path
//! derivation and acquisition semantics — the ADR requires this, because
//! a divergence between them (or between this and Circinus's Python
//! implementation) is "indistinguishable from no lock while looking like
//! one" (ADR-0014 section 12).
//!
//! # What this module does and does not do
//!
//! - Derives the lock sidecar path from a configuration file's path
//!   (section 1).
//! - Acquires an exclusive `flock(2)` advisory lock on that sidecar by
//!   non-blocking poll with bounded backoff (section 3).
//! - Returns a guard that releases the lock on drop (section 3, step
//!   10).
//!
//! It never reads, writes, truncates, renames, or unlinks the sidecar's
//! *content* — the sidecar is a zero-byte coordination file, and its
//! presence or content is never part of the protocol (section 6, section
//! 11).
//!
//! # Why `flock(2)` and not `fcntl(F_SETLK)`
//!
//! POSIX record locks (`fcntl`) are process-scoped: they are released
//! when *any* file descriptor to the locked file is closed anywhere in
//! the process, even one opened independently of the lock holder. That
//! is a silent correctness hazard this ADR exists to avoid (section 3).
//! `rustix::fs::flock` calls the real `flock(2)` syscall on Unix —
//! open-file-description-scoped, not process-scoped — which is why it
//! was chosen here (see the PR description for the crate-selection
//! rationale, including the empirical test in this module's test suite
//! that would fail if this were accidentally an `fcntl` record lock
//! instead).

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::{flock, FlockOperation};
use rustix::io::Errno;

/// Appended to a configuration file's complete name (including its
/// extension) to derive its lock sidecar path. ADR-0014 section 1: this
/// must be appended, never used to replace the extension.
const LOCK_SUFFIX: &str = ".horonom-write.lock";

/// Total acquisition budget (ADR-0014 section 3), measured on a
/// monotonic clock.
const BUDGET: Duration = Duration::from_millis(5000);

/// Why lock acquisition failed. ADR-0014 section 5 requires class A
/// (contention — the budget was exhausted while another holder kept the
/// lock) and class B (any other `open`/`flock` error — `ENOTSUP`,
/// `EACCES`, `EROFS`, etc.) to be distinguishable by the caller and by
/// tests; collapsing them into one variant is called out there as a
/// conformance failure. This type carries that distinction; each
/// product's own error enum wraps it in a single new variant (see
/// `SettingsError::WriteLockUnavailable` /
/// `CodexHooksError::WriteLockUnavailable`) that forwards the display
/// text and lets `matches!` on the wrapped `LockFailure` recover the
/// class when a caller needs to.
#[derive(Debug)]
pub enum LockFailure {
    /// Class A: the 5000 ms budget was exhausted while `EWOULDBLOCK` /
    /// `EAGAIN` kept being returned — another process holds the lock.
    Contention { waited_ms: u64, attempts: u32 },
    /// Class B: `open(2)` or `flock(2)` failed with anything else
    /// (`ENOTSUP`/`EOPNOTSUPP`/`ENOLCK`/`EACCES`/`EPERM`/`EROFS`/other).
    /// Never retried (ADR-0014 section 3).
    Error { errno: Option<i32>, message: String },
}

impl std::error::Error for LockFailure {}

impl std::fmt::Display for LockFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockFailure::Contention {
                waited_ms,
                attempts,
            } => write!(
                f,
                "another Horonom writer is updating this file; timed out after {waited_ms}ms \
                 ({attempts} attempts) — retry in a moment"
            ),
            LockFailure::Error { errno, message } => match errno {
                Some(e) => write!(f, "could not acquire the write lock (errno {e}): {message}"),
                None => write!(f, "could not acquire the write lock: {message}"),
            },
        }
    }
}

/// An acquired lock. Releases on drop (closes the file descriptor),
/// never renames, truncates, or unlinks the sidecar (ADR-0014 section
/// 6).
pub struct WriteLockGuard {
    // Held only to keep the file descriptor (and therefore the lock)
    // alive until this guard is dropped. Never read from or written to.
    _file: File,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) lock_path: PathBuf,
}

/// Derives `canonical_parent(config_path) / (basename(config_path) +
/// ".horonom-write.lock")` (ADR-0014 section 1). The parent directory
/// must already exist — the caller creates it first (ADR-0014 section
/// 3, step 1), before this function canonicalizes it. Never
/// canonicalizes `config_path` itself.
pub fn derive_lock_path(config_path: &Path) -> std::io::Result<PathBuf> {
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    let canonical_parent = std::fs::canonicalize(parent)?;
    let file_name = config_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    let mut lock_file_name = file_name;
    lock_file_name.push(LOCK_SUFFIX);
    Ok(canonical_parent.join(lock_file_name))
}

/// Acquires an exclusive `flock(2)` lock on the sidecar derived from
/// `config_path`, by non-blocking poll with bounded backoff (ADR-0014
/// section 3). The caller must have already ensured `config_path`'s
/// parent directory exists.
///
/// Opens the sidecar with `O_CREAT | O_RDWR`, mode `0600` **when
/// creating** — never `O_TRUNC`, never `O_APPEND`, never `chmod` an
/// existing sidecar (ADR-0014 section 3, step 3; the explicit `.mode()`
/// on `OpenOptions` is required because the default is `0666 &
/// ~umask`).
///
/// Poll schedule (ADR-0014 section 3, normative): attempt 1 immediate at
/// `t = 0`; backoff before attempt `n+1` is `min(10ms * 2^(n-1),
/// 250ms)`, i.e. `10, 20, 40, 80, 160, 250, 250, ...`; after a failed
/// attempt, if elapsed >= 5000ms stop and time out, otherwise sleep
/// `min(delay, 5000ms - elapsed)` and retry, guaranteeing a final
/// attempt near the deadline.
///
/// `EINTR` is retried immediately without consuming a backoff step
/// (ADR-0014 section 3 — Rust-only clause; Python's `fcntl.flock`
/// already retries `EINTR` internally per PEP 475). `EWOULDBLOCK` /
/// `EAGAIN` are retried per the schedule. Any other error is not
/// retried and fails immediately as [`LockFailure::Error`].
pub fn acquire(config_path: &Path) -> Result<WriteLockGuard, LockFailure> {
    let lock_path = derive_lock_path(config_path).map_err(|e| LockFailure::Error {
        errno: e.raw_os_error(),
        message: format!("could not canonicalize the lock sidecar's parent directory: {e}"),
    })?;

    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| LockFailure::Error {
            errno: e.raw_os_error(),
            message: format!("could not open lock sidecar {}: {e}", lock_path.display()),
        })?;

    let start = Instant::now();
    let mut attempt: u32 = 0;

    loop {
        attempt += 1;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {
                return Ok(WriteLockGuard {
                    _file: file,
                    lock_path,
                });
            }
            Err(Errno::INTR) => {
                // Retry immediately — does not consume a backoff step.
                continue;
            }
            // EAGAIN and EWOULDBLOCK are the same value on every
            // platform this crate targets (Linux, macOS) — matching
            // both arms would trip clippy's unreachable-pattern lint
            // under `-D warnings`. EWOULDBLOCK is `flock(2)`'s
            // documented errno for "already locked"; matched here for
            // that reason.
            Err(Errno::WOULDBLOCK) => {
                let elapsed = start.elapsed();
                if elapsed >= BUDGET {
                    return Err(LockFailure::Contention {
                        waited_ms: elapsed.as_millis() as u64,
                        attempts: attempt,
                    });
                }
                let backoff_ms = 10u64.saturating_mul(1u64 << (attempt.saturating_sub(1)));
                let backoff = Duration::from_millis(backoff_ms.min(250));
                let remaining = BUDGET - elapsed;
                std::thread::sleep(backoff.min(remaining));
            }
            Err(other) => {
                return Err(LockFailure::Error {
                    errno: Some(other.raw_os_error()),
                    message: other.to_string(),
                });
            }
        }
    }
}

/// Hidden `__lock_test_hold` subcommand — see `main.rs`'s dispatch arm
/// for why this exists and what it prints. Only ever invoked by
/// `tests/write_lock_cross_process.rs` as a genuinely separate OS
/// process; never reachable from documented CLI usage.
pub mod test_hold_cmd {
    use std::io::Write as _;

    pub fn run(config_path: &str, hold_ms: &str) {
        let path = std::path::PathBuf::from(config_path);
        let hold_ms: u64 = hold_ms.parse().unwrap_or(0);
        match super::acquire(&path) {
            Ok(guard) => {
                println!("LOCKED");
                std::io::stdout().flush().ok();
                std::thread::sleep(std::time::Duration::from_millis(hold_ms));
                drop(guard);
            }
            Err(e) => {
                println!("LOCK_FAILED: {e}");
                std::io::stdout().flush().ok();
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_lock_path_appends_suffix_to_full_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("settings.json");
        let lock_path = derive_lock_path(&config_path).unwrap();
        assert_eq!(
            lock_path.file_name().unwrap().to_str().unwrap(),
            "settings.json.horonom-write.lock",
            "must append to the full name including extension, never replace it \
             (Path::with_extension would wrongly yield settings.horonom-write.lock)"
        );
    }

    #[test]
    fn derive_lock_path_canonicalizes_the_parent_not_the_config_file() {
        let real_dir = tempfile::tempdir().unwrap();
        let link_dir = real_dir.path().parent().unwrap().join(format!(
            "link-{}",
            real_dir.path().file_name().unwrap().to_str().unwrap()
        ));
        std::os::unix::fs::symlink(real_dir.path(), &link_dir).unwrap();

        let via_link = derive_lock_path(&link_dir.join("settings.json")).unwrap();
        let via_real = derive_lock_path(&real_dir.path().join("settings.json")).unwrap();
        assert_eq!(
            via_link, via_real,
            "two textually different but same-inode parents must converge on one sidecar"
        );

        std::fs::remove_file(&link_dir).unwrap();
    }

    #[test]
    fn acquire_succeeds_when_nothing_else_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("settings.json");
        let guard = acquire(&config_path).unwrap();
        assert!(guard
            .lock_path
            .ends_with("settings.json.horonom-write.lock"));
    }

    #[test]
    fn acquire_creates_the_sidecar_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("settings.json");
        let guard = acquire(&config_path).unwrap();
        let mode = std::fs::metadata(&guard.lock_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_second_open_file_description_in_the_same_process_cannot_reacquire() {
        // Empirical proof this is flock(2), not fcntl(F_SETLK): a POSIX
        // record lock is process-scoped and a second fd in the same
        // process would succeed in acquiring it. flock(2) is
        // open-file-description-scoped, so it must fail here exactly as
        // a foreign process would (ADR-0014 section 3, "One lock at a
        // time").
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("settings.json");
        let lock_path = derive_lock_path(&config_path).unwrap();
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();

        let first = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&lock_path)
            .unwrap();
        flock(&first, FlockOperation::NonBlockingLockExclusive).unwrap();

        let second = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&lock_path)
            .unwrap();
        let result = flock(&second, FlockOperation::NonBlockingLockExclusive);
        assert!(
            matches!(result, Err(Errno::WOULDBLOCK)),
            "a second open-file-description lock attempt on the same path, even from this \
             same process, must fail exactly like a foreign holder would"
        );

        drop(first);
    }

    #[test]
    fn releasing_the_guard_lets_a_subsequent_acquire_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("settings.json");
        let guard = acquire(&config_path).unwrap();
        drop(guard);
        let second = acquire(&config_path);
        assert!(second.is_ok());
    }
}
