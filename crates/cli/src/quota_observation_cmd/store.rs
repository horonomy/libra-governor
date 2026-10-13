//! On-disk registration store for the quota-observation operator CLI
//! (HORO-1870 PR2): resolves the canonical registration directory, loads
//! registrations with their file paths (unlike
//! `libra_governor_quota_observation::registration::load_registrations`,
//! which deliberately discards paths once a directory is known-valid as a
//! whole), and writes one registration back atomically under the
//! daemon's own cross-process write lock.
//!
//! # Why this module re-scans rather than reusing `load_registrations` alone
//!
//! `load_registrations` is the right primitive for "give me every valid,
//! pool-unique registration in this directory" — but it intentionally
//! returns `Vec<Registration>`, not `Vec<(PathBuf, Registration)>`,
//! because nothing in the crate's existing callers (the runtime that
//! actually invokes an extension) needs to know which file a registration
//! came from. This CLI does: `approve`/`enable`/`disable`/`uninstall` all
//! need to find and rewrite the *specific file* a `registration_id`
//! lives in. [`load_with_paths`] mirrors `load_registrations`'s own
//! loop (sorted `*.json` entries, fail-closed on any unparseable or
//! invalid file, `check_pool_uniqueness` over the whole directory) but
//! keeps the path alongside each entry.

use std::fs::{self, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use libra_governor_daemon::write_lock::{self, LockFailure, WriteLockGuard};
use libra_governor_quota_observation::registration::{
    check_pool_uniqueness, Registration, RegistrationError,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("could not determine the Libra Governor state directory: {0}")]
    StateDir(#[from] libra_governor_daemon::paths::PathsError),
    #[error("could not create or harden registration directory {0:?}: {1}")]
    EnsureDir(PathBuf, String),
    #[error("registration_id {0:?} must not be empty or contain '/'")]
    InvalidRegistrationId(String),
    #[error("a registration with registration_id {0:?} already exists at {1:?}")]
    AlreadyExists(String, PathBuf),
    #[error("no registration with registration_id {0:?} was found in {1:?}")]
    NotFound(String, PathBuf),
    #[error(transparent)]
    Registration(#[from] RegistrationError),
    #[error("could not acquire the quota-observation registration lock: {0}")]
    Lock(#[from] LockFailure),
    #[error("failed to write registration file {0:?}: {1}")]
    Write(PathBuf, String),
    #[error("failed to delete registration file {0:?}: {1}")]
    Delete(PathBuf, String),
    #[error("failed to serialize registration: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("failed to measure {0:?}'s identity before approval: {1}")]
    Measurement(String, String),
    #[error("failed to approve {0:?}: {1}")]
    Approval(String, String),
}

/// `$STATE_DIR/quota-observation/registrations` — the canonical directory
/// every quota-observation CLI command reads and writes. Does not create
/// it; see [`ensure_registrations_dir`].
pub fn registrations_dir() -> Result<PathBuf, StoreError> {
    Ok(libra_governor_daemon::paths::state_dir()?
        .join("quota-observation")
        .join("registrations"))
}

/// [`registrations_dir`], created (and, like
/// `libra_governor_daemon::paths::ensure_state_dir`, idempotently
/// tightened) to mode `0700` — this directory holds the approved
/// executable path and the bound trust digests for every registered
/// extension, which is exactly the kind of local-multi-user disclosure
/// surface `ensure_state_dir`'s own doc comment already calls out for the
/// daemon's state directory.
fn ensure_registrations_dir() -> Result<PathBuf, StoreError> {
    let dir = registrations_dir()?;
    if !dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| StoreError::EnsureDir(dir.clone(), e.to_string()))?;
    } else {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| StoreError::EnsureDir(dir.clone(), e.to_string()))?;
    }
    Ok(dir)
}

/// Acquires the directory-level write lock for a mutating operation.
/// Reuses `libra_governor_daemon::write_lock::acquire` — the same
/// primitive and lock semantics `claude_settings.rs`/`codex_hooks_file.rs`
/// already use for their own host-local config writers — rather than a
/// second lock implementation. The lock sidecar is derived from the
/// registrations *directory* path itself (a sibling file named
/// `registrations.horonom-write.lock` under `quota-observation/`), since
/// every mutating command in this family touches the directory as a
/// whole (adding, removing, or rewriting one member file), not a single
/// fixed file.
fn acquire_lock(dir: &Path) -> Result<WriteLockGuard, StoreError> {
    write_lock::acquire(dir).map_err(StoreError::from)
}

/// Loads every `*.json` registration in `dir`, sorted by file name,
/// alongside its path — fail-closed on any unparseable/invalid file or a
/// cross-file duplicate pool binding, exactly like
/// `registration::load_registrations`, whose loop this mirrors. Returns
/// an empty list (not an error) when `dir` does not exist yet — a fresh
/// install with no registrations is not a loader error.
pub fn load_with_paths(dir: &Path) -> Result<Vec<(PathBuf, Registration)>, StoreError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let entries =
        fs::read_dir(dir).map_err(|e| StoreError::EnsureDir(dir.to_owned(), e.to_string()))?;
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| StoreError::EnsureDir(dir.to_owned(), e.to_string()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    paths.sort();

    let mut loaded = Vec::with_capacity(paths.len());
    for path in paths {
        let raw = fs::read_to_string(&path)
            .map_err(|e| RegistrationError::FileRead(path.clone(), e.to_string()))?;
        let registration: Registration = serde_json::from_str(&raw)
            .map_err(|e| RegistrationError::Parse(path.clone(), e.to_string()))?;
        registration.validate()?;
        loaded.push((path, registration));
    }
    check_pool_uniqueness(&loaded)?;
    Ok(loaded)
}

/// Rejects a `registration_id` that could not safely become a file name
/// in the registrations directory (path traversal, empty, or a dotfile
/// alias for the directory itself).
fn validate_registration_id(id: &str) -> Result<(), StoreError> {
    if id.is_empty() || id.contains('/') || id == "." || id == ".." {
        return Err(StoreError::InvalidRegistrationId(id.to_owned()));
    }
    Ok(())
}

fn file_path_for(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// Writes `registration` to `path` atomically (temp file in the same
/// directory, fsynced, then renamed over any existing file of the same
/// name) with mode `0600`. Callers must already hold the directory write
/// lock returned by [`acquire_lock`] — enforced by every public entry
/// point in this module taking a `&WriteLockGuard`, not by caller
/// discipline alone.
fn write_atomically(
    _guard: &WriteLockGuard,
    path: &Path,
    registration: &Registration,
) -> Result<(), StoreError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let json = serde_json::to_string_pretty(registration)?;
    let tmp_path = dir.join(format!(
        ".{}.quota-obs-tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "registration.json".to_string()),
        std::process::id()
    ));
    let write_result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp_path)?;
        file.write_all(json.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        // `OpenOptions::mode` only governs a freshly created file; a
        // pre-existing temp name from a crashed previous attempt could
        // have looser permissions, so it is set explicitly rather than
        // trusted.
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp_path, path)
    })();
    write_result.map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        StoreError::Write(path.to_owned(), e.to_string())
    })
}

/// `register`: validates `candidate` against the current directory
/// contents (schema/semantic validation, a fresh `registration_id`, and
/// pool uniqueness including the candidate), then writes it to
/// `<dir>/<registration_id>.json` atomically under the directory lock.
pub fn register(candidate: Registration) -> Result<PathBuf, StoreError> {
    validate_registration_id(&candidate.registration_id)?;
    candidate.validate()?;

    let dir = ensure_registrations_dir()?;
    let guard = acquire_lock(&dir)?;

    let existing = load_with_paths(&dir)?;
    if existing
        .iter()
        .any(|(_, r)| r.registration_id == candidate.registration_id)
    {
        let path = file_path_for(&dir, &candidate.registration_id);
        return Err(StoreError::AlreadyExists(candidate.registration_id, path));
    }

    let path = file_path_for(&dir, &candidate.registration_id);
    let mut combined = existing;
    combined.push((path.clone(), candidate.clone()));
    check_pool_uniqueness(&combined)?;

    write_atomically(&guard, &path, &candidate)?;
    Ok(path)
}

/// Loads the one registration (with its path) matching `id`, failing
/// closed over the whole directory exactly like [`load_with_paths`] —
/// used by every read-mostly command (`doctor`, `inspect`) as well as the
/// mutating ones below, which re-run it while holding the lock so their
/// read reflects the locked, authoritative state.
pub fn find(dir: &Path, id: &str) -> Result<(PathBuf, Registration), StoreError> {
    let loaded = load_with_paths(dir)?;
    loaded
        .into_iter()
        .find(|(_, r)| r.registration_id == id)
        .ok_or_else(|| StoreError::NotFound(id.to_owned(), dir.to_owned()))
}

/// Runs `mutate` against the current on-disk registration for `id` while
/// holding the directory write lock, then writes the result back
/// atomically to the same file. `mutate` receives the freshly re-loaded
/// registration (never a caller-cached copy), so every mutating command
/// in this module always edits the authoritative on-disk state.
fn with_locked_registration(
    id: &str,
    mutate: impl FnOnce(Registration) -> Result<Registration, StoreError>,
) -> Result<Registration, StoreError> {
    let dir = ensure_registrations_dir()?;
    let guard = acquire_lock(&dir)?;
    let (path, registration) = find(&dir, id)?;
    let mutated = mutate(registration)?;
    write_atomically(&guard, &path, &mutated)?;
    Ok(mutated)
}

/// `approve`: the operator-facing replacement for the test-only
/// convergence loop. Measures `registration`'s identity fresh from disk
/// and binds the result, whether this is a first-time approval
/// (`approved: false`) or a re-approval after a legitimate change.
pub fn approve(id: &str) -> Result<Registration, StoreError> {
    with_locked_registration(id, |registration| {
        let measured = libra_governor_quota_observation::identity::measure_identity(&registration)
            .map_err(|e| StoreError::Measurement(id.to_owned(), e.to_string()))?;
        libra_governor_quota_observation::identity::approve(registration, measured)
            .map_err(|e| StoreError::Approval(id.to_owned(), e.to_string()))
    })
}

/// `enable`/`disable`: flips `Registration.enabled` only. Never touches
/// any approval digest — re-enabling after a disable with nothing else
/// changed reproduces the same manifest digest the registration was
/// approved under (see `trust-and-lifecycle.md`'s own framing), so no
/// re-approval is required.
pub fn set_enabled(id: &str, enabled: bool) -> Result<Registration, StoreError> {
    with_locked_registration(id, |mut registration| {
        registration.enabled = enabled;
        Ok(registration)
    })
}

/// `uninstall`: deletes exactly `<dir>/<registration_id>.json` under the
/// directory lock. Never touches the extension's own executable or
/// runtime files — those are the operator's own separate concern (see
/// `trust-and-lifecycle.md`'s "Uninstall" section).
pub fn uninstall(id: &str) -> Result<(PathBuf, Registration), StoreError> {
    let dir = ensure_registrations_dir()?;
    let guard = acquire_lock(&dir)?;
    let (path, registration) = find(&dir, id)?;
    fs::remove_file(&path).map_err(|e| StoreError::Delete(path.clone(), e.to_string()))?;
    drop(guard);
    Ok((path, registration))
}
