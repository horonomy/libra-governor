//! Trusted-mode startup checks (ADR-0019 §1 "Launch and startup checks",
//! design doc §3). Every check runs in order; the first failure returns
//! and nothing is created on disk before step 4 (the state/socket dir
//! checks) passes.

use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use libra_governor_ledger::{LedgerStore, APPEND_ONLY_TRIGGERS};

use super::TrustedPrincipalConfig;

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error(
        "trusted mode refuses to start: this process's real and effective uid must equal \
             the configured provider_uid, and that uid must not be 0"
    )]
    ProcessIdentity,
    #[error(
        "trusted mode refuses to start: {path} is not owned by root or the provider, or is \
             group- or other-writable"
    )]
    PathChain { path: PathBuf },
    #[error(
        "trusted mode refuses to start: the state dir must be owned by the provider with \
             mode 0700"
    )]
    StateDir,
    #[error(
        "trusted mode refuses to start: the socket dir must be owned by the provider, must \
             not be other-accessible, and must not be writable by group or other"
    )]
    SocketDir,
    #[error("trusted mode refuses to start: ledger error: {0}")]
    Ledger(#[from] libra_governor_ledger::LedgerError),
    #[error("trusted mode refuses to start: missing required append-only trigger {0}")]
    MissingTrigger(&'static str),
    #[error("trusted mode refuses to start: io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Runs ADR-0019 §1's startup checks in order, returning the
/// already-migrated ledger. The caller binds the socket only after this
/// succeeds, and `serve_trusted` reuses this same `LedgerStore` rather
/// than reopening it.
///
/// The config FILE's own ownership/mode is deliberately not re-checked
/// here by path — [`TrustedPrincipalConfig::load`] already verified that
/// atomically against the same open handle it read from, closing a
/// check-then-reopen TOCTOU window (HORO-1835 security review). This
/// function's `check_path_chain` still separately walks every ANCESTOR
/// directory of `config_path` (and of the executable, state dir, and
/// socket dir).
pub fn validate_trusted_startup(
    cfg: &TrustedPrincipalConfig,
    config_path: &Path,
) -> Result<LedgerStore, StartupError> {
    check_process_identity(cfg)?;
    for path in [
        std::env::current_exe().map_err(StartupError::Io)?,
        config_path.to_path_buf(),
        cfg.state_dir.clone(),
        cfg.socket_dir.clone(),
    ] {
        check_path_chain(&path, &[0, cfg.provider_uid])?;
    }
    check_state_dir(cfg)?;
    check_socket_dir(cfg)?;

    let ledger = LedgerStore::open(cfg.ledger_path())?;
    check_append_only_triggers(&ledger)?;
    Ok(ledger)
}

fn check_process_identity(cfg: &TrustedPrincipalConfig) -> Result<(), StartupError> {
    let real = rustix::process::getuid().as_raw();
    let effective = rustix::process::geteuid().as_raw();
    if real == 0 || real != cfg.provider_uid || effective != cfg.provider_uid {
        return Err(StartupError::ProcessIdentity);
    }
    Ok(())
}

/// One component of the path chain, reduced to exactly what
/// [`evaluate_chain`] needs to judge it — probed via `symlink_metadata`,
/// never dereferenced as the agent's own choice of path (these are
/// root-pinned paths: the executable, the config, the state dir, the
/// socket dir — never an agent-named one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileKind {
    Dir,
    File,
    Other,
}

fn kind_of(metadata: &Metadata) -> FileKind {
    if metadata.is_dir() {
        FileKind::Dir
    } else if metadata.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    }
}

/// Canonicalizes `path`, then walks every component from the leaf up to
/// `/` with `symlink_metadata`, requiring each to be owned by one of
/// `owners` and not group- or other-writable.
///
/// Canonicalizing first is required because macOS's `/etc` and `/var`
/// are root-owned symlinks into `/private` — safe here because every
/// resolved ancestor is then verified non-writable by the agent, and
/// because these are root-pinned paths, not agent-named ones (canonicalizing
/// an agent-named path would be the confused-deputy read `serve.rs`'s
/// sanitizer exists to prevent).
///
/// `pub`, not `pub(crate)`: PR-2c's client-side pinned-endpoint
/// resolution (`crates/cli/src/trusted_client.rs`) reuses this exact
/// function (design doc §5: "canonicalize, then
/// `check_path_chain(path, &[0])`") rather than duplicating the walk.
pub fn check_path_chain(path: &Path, owners: &[u32]) -> Result<(), StartupError> {
    let canonical = path.canonicalize().map_err(|_| StartupError::PathChain {
        path: path.to_path_buf(),
    })?;
    let mut components: Vec<(PathBuf, u32, u32, FileKind)> = Vec::new();
    let mut current = canonical.clone();
    loop {
        let metadata =
            std::fs::symlink_metadata(&current).map_err(|_| StartupError::PathChain {
                path: current.clone(),
            })?;
        components.push((
            current.clone(),
            metadata.uid(),
            metadata.mode(),
            kind_of(&metadata),
        ));
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    evaluate_chain(&components, owners).map_err(|_| StartupError::PathChain { path: canonical })
}

/// Testability split (design doc §3): a pure evaluator over already-read
/// probe results, unit-testable without root, on Linux and macOS. The
/// real probe ([`check_path_chain`]) is exercised by the harness
/// container, where every path really is owned the way production
/// expects.
pub(crate) fn evaluate_chain(
    components: &[(PathBuf, u32, u32, FileKind)],
    owners: &[u32],
) -> Result<(), ()> {
    for (_path, uid, mode, _kind) in components {
        if !owners.contains(uid) || mode & 0o022 != 0 {
            return Err(());
        }
    }
    Ok(())
}

fn check_state_dir(cfg: &TrustedPrincipalConfig) -> Result<(), StartupError> {
    let metadata = std::fs::symlink_metadata(&cfg.state_dir).map_err(|_| StartupError::StateDir)?;
    if !metadata.is_dir() || metadata.uid() != cfg.provider_uid || metadata.mode() & 0o7777 != 0o700
    {
        return Err(StartupError::StateDir);
    }
    Ok(())
}

fn check_socket_dir(cfg: &TrustedPrincipalConfig) -> Result<(), StartupError> {
    let metadata =
        std::fs::symlink_metadata(&cfg.socket_dir).map_err(|_| StartupError::SocketDir)?;
    // No group- or other-WRITE (mode & 0o022 == 0), and no OTHER access
    // at all (mode & 0o007 == 0). This accepts 2710 (owner rwx, group
    // --x traverse-only, other none) without requiring the exact byte
    // value — ADR-0019 §1's "group bits exception" is just the
    // group-traverse bit.
    if !metadata.is_dir()
        || metadata.uid() != cfg.provider_uid
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o007 != 0
    {
        return Err(StartupError::SocketDir);
    }
    Ok(())
}

fn check_append_only_triggers(ledger: &LedgerStore) -> Result<(), StartupError> {
    let present = ledger.trigger_names()?;
    for required in APPEND_ONLY_TRIGGERS {
        if !present.iter().any(|name| name == required) {
            return Err(StartupError::MissingTrigger(required));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn evaluate_chain_accepts_root_and_provider_owned_non_writable_components() {
        let components = vec![
            (PathBuf::from("/"), 0, 0o755, FileKind::Dir),
            (PathBuf::from("/var"), 0, 0o755, FileKind::Dir),
            (PathBuf::from("/var/lib"), 0, 0o755, FileKind::Dir),
            (
                PathBuf::from("/var/lib/libra-governor"),
                10001,
                0o700,
                FileKind::Dir,
            ),
        ];
        assert!(evaluate_chain(&components, &[0, 10001]).is_ok());
    }

    #[test]
    fn evaluate_chain_rejects_an_agent_owned_component() {
        let components = vec![
            (PathBuf::from("/"), 0, 0o755, FileKind::Dir),
            (PathBuf::from("/tmp"), 10002, 0o755, FileKind::Dir),
        ];
        assert!(evaluate_chain(&components, &[0, 10001]).is_err());
    }

    #[test]
    fn evaluate_chain_rejects_a_group_writable_component() {
        let components = vec![(PathBuf::from("/"), 0, 0o775, FileKind::Dir)];
        assert!(evaluate_chain(&components, &[0, 10001]).is_err());
    }

    #[test]
    fn evaluate_chain_rejects_an_other_writable_component() {
        let components = vec![(PathBuf::from("/"), 0, 0o757, FileKind::Dir)];
        assert!(evaluate_chain(&components, &[0, 10001]).is_err());
    }

    #[test]
    fn evaluate_chain_accepts_the_socket_dirs_group_traverse_only_exception() {
        // 2710: owner rwx, group --x (traverse only), other none. Group
        // bit 1 (--x) does not set the write bit, so this passes the
        // same no-group/other-WRITE rule the path chain enforces —
        // `check_socket_dir`'s own, slightly different rule (also
        // requiring zero OTHER access) is tested separately below.
        let components = vec![(
            PathBuf::from("/run/libra-governor"),
            10001,
            0o2710,
            FileKind::Dir,
        )];
        assert!(evaluate_chain(&components, &[0, 10001]).is_ok());
    }

    #[test]
    fn check_state_dir_rejects_a_too_permissive_mode() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = TrustedPrincipalConfig {
            provider_uid: rustix::process::getuid().as_raw(),
            agent_uids: vec![rustix::process::getuid().as_raw() + 1],
            provider_id: "operator:lg-provider".to_string(),
            state_dir: state_dir.clone(),
            socket_dir: dir.path().join("socket"),
            connection_timeout_ms: 5000,
        };
        assert!(matches!(check_state_dir(&cfg), Err(StartupError::StateDir)));
    }

    #[test]
    fn check_socket_dir_rejects_other_accessible_mode() {
        let dir = tempfile::tempdir().unwrap();
        let socket_dir = dir.path().join("socket");
        std::fs::create_dir(&socket_dir).unwrap();
        std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o2715)).unwrap();
        let cfg = TrustedPrincipalConfig {
            provider_uid: rustix::process::getuid().as_raw(),
            agent_uids: vec![rustix::process::getuid().as_raw() + 1],
            provider_id: "operator:lg-provider".to_string(),
            state_dir: dir.path().join("state"),
            socket_dir,
            connection_timeout_ms: 5000,
        };
        assert!(matches!(
            check_socket_dir(&cfg),
            Err(StartupError::SocketDir)
        ));
    }

    #[test]
    fn check_append_only_triggers_passes_against_a_freshly_migrated_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = LedgerStore::open(dir.path().join("ledger.sqlite3")).unwrap();
        assert!(check_append_only_triggers(&ledger).is_ok());
    }

    // The negative case — root drops a trigger offline, daemon refuses to
    // start — needs direct SQL access `LedgerStore` deliberately does not
    // expose publicly (it is `pub(crate)`-only inside the ledger crate).
    // That regression is proven at two layers instead: a ledger-crate
    // unit test (`crates/ledger/src/store.rs`'s
    // `a_dropped_trigger_is_reported_missing_after_reopening_a_file_db`)
    // confirms `trigger_names()` itself reports the gap, and the harness
    // test `x_startup_refuses_missing_append_only_trigger`
    // (`x_startup_checks.rs`) confirms this function's caller
    // (`validate_trusted_startup`) refuses to start against the real
    // on-disk ledger file after `sqlite3 ... DROP TRIGGER`.
}
