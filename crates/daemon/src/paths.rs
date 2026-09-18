//! Resolves where the daemon's on-disk state lives: its Unix socket, its
//! SQLite ledger file, and its log file.
//!
//! Resolution order for the state directory:
//! 1. `LIBRA_GOVERNOR_STATE_DIR` env var, if set (primarily for tests and
//!    for running more than one daemon side by side).
//! 2. `$XDG_STATE_HOME/libra-governor` (XDG Base Directory convention).
//! 3. `$HOME/.local/state/libra-governor` (the XDG default when
//!    `XDG_STATE_HOME` is unset — used as-is on macOS too, rather than
//!    `~/Library/Application Support`, so the same layout works
//!    identically across the Unix-like platforms Libra targets in MVP
//!    1.0).
//!
//! The directory (and its parents) is created on demand by [`ensure_dir`]
//! — callers should not assume it pre-exists.

use std::path::PathBuf;

/// Errors resolving or preparing the state directory.
#[derive(Debug, thiserror::Error)]
pub enum PathsError {
    #[error("could not determine home directory (HOME env var unset)")]
    NoHomeDir,
    #[error("io error preparing state dir: {0}")]
    Io(#[from] std::io::Error),
}

/// Resolves the state directory per the order documented on this module,
/// without creating it.
pub fn state_dir() -> Result<PathBuf, PathsError> {
    if let Ok(dir) = std::env::var("LIBRA_GOVERNOR_STATE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Ok(xdg_state_home) = std::env::var("XDG_STATE_HOME") {
        return Ok(PathBuf::from(xdg_state_home).join("libra-governor"));
    }
    let home = std::env::var("HOME").map_err(|_| PathsError::NoHomeDir)?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("libra-governor"))
}

/// [`state_dir`], creating it (and its parents) if it does not already
/// exist.
///
/// Created (and, if it already existed from before this hardening
/// landed, tightened) to mode `0700` — owner-only — since this directory
/// holds the SQLite ledger, the daemon's Unix socket, and the gateway
/// capability token: real local-multi-user information-disclosure
/// surface otherwise (HORO-1146 security review finding #5). This is the
/// actual security boundary for everything under it; per-file hardening
/// on the ledger/socket below is defense in depth on top of this, not a
/// substitute for it — the ledger's own `-wal`/`-shm` sidecar files, for
/// instance, are protected by this directory mode rather than hardened
/// individually.
pub fn ensure_state_dir() -> Result<PathBuf, PathsError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let dir = state_dir()?;
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
    } else {
        // `DirBuilder::mode` only governs newly created directories — a
        // directory that predates this hardening (or was otherwise
        // created with a looser mode) is tightened here idempotently,
        // rather than only ever protecting a fresh install.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// The daemon's Unix domain socket path.
pub fn socket_path() -> Result<PathBuf, PathsError> {
    Ok(state_dir()?.join("daemon.sock"))
}

/// The daemon's SQLite ledger file path.
pub fn ledger_path() -> Result<PathBuf, PathsError> {
    Ok(state_dir()?.join("ledger.sqlite3"))
}

/// The daemon's log file path. Never receives raw prompt text or hook
/// payload content — see `crates/daemon/src/log.rs`.
pub fn log_path() -> Result<PathBuf, PathsError> {
    Ok(state_dir()?.join("daemon.log"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // std::env::set_var is process-global; serialize the tests that touch
    // it so they cannot observe each other's env mutations.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn state_dir_honors_explicit_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("LIBRA_GOVERNOR_STATE_DIR", "/tmp/libra-test-override");
        let dir = state_dir().unwrap();
        std::env::remove_var("LIBRA_GOVERNOR_STATE_DIR");
        assert_eq!(dir, PathBuf::from("/tmp/libra-test-override"));
    }

    #[test]
    fn socket_and_ledger_paths_share_the_state_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("LIBRA_GOVERNOR_STATE_DIR", "/tmp/libra-test-shared");
        let socket = socket_path().unwrap();
        let ledger = ledger_path().unwrap();
        std::env::remove_var("LIBRA_GOVERNOR_STATE_DIR");
        assert_eq!(socket.parent(), ledger.parent());
        assert_eq!(socket.file_name().unwrap(), "daemon.sock");
        assert_eq!(ledger.file_name().unwrap(), "ledger.sqlite3");
    }

    #[test]
    fn ensure_state_dir_creates_directory() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("nested").join("state");
        std::env::set_var("LIBRA_GOVERNOR_STATE_DIR", &target);
        let created = ensure_state_dir().unwrap();
        std::env::remove_var("LIBRA_GOVERNOR_STATE_DIR");
        assert!(created.is_dir());
        assert_eq!(created, target);
    }
}
