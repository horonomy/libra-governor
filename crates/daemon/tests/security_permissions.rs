//! HORO-1146 security review finding #5 regression: the daemon's state
//! directory, Unix socket, and SQLite ledger file (main + WAL sidecars)
//! must be created owner-only, not left at the umask-derived default
//! (`755`/`644` observed in the gate's own security review). Asserts the
//! REAL file mode after a REAL `ensure_state_dir` / `bind_or_detect_running`
//! / `LedgerStore::open` call — the same functions `daemon run`'s startup
//! path calls — not a source-reading-only claim.

use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

use libra_governor_ledger::LedgerStore;

// `LIBRA_GOVERNOR_STATE_DIR` is process-global; serialize the tests in
// this file that set it so they cannot observe each other's env
// mutations (mirrors `paths.rs`'s own `ENV_LOCK` pattern).
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn mode_of(path: &std::path::Path) -> u32 {
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("could not stat {path:?}: {e}"))
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn ensure_state_dir_creates_the_directory_owner_only() {
    let _guard = ENV_LOCK.lock().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let target = parent.path().join("fresh-state-dir");
    std::env::set_var("LIBRA_GOVERNOR_STATE_DIR", &target);

    let dir = libra_governor_daemon::paths::ensure_state_dir().unwrap();
    std::env::remove_var("LIBRA_GOVERNOR_STATE_DIR");

    assert_eq!(
        mode_of(&dir),
        0o700,
        "a freshly created state dir must be owner-only (0700), not the umask default"
    );
}

#[test]
fn ensure_state_dir_tightens_an_existing_directory_created_with_a_looser_mode() {
    let _guard = ENV_LOCK.lock().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let target = parent.path().join("preexisting-state-dir");
    // Simulate a pre-HORO-1146 install: the directory already exists at
    // the old umask-derived 0755.
    std::fs::create_dir_all(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(mode_of(&target), 0o755, "test setup sanity check");

    std::env::set_var("LIBRA_GOVERNOR_STATE_DIR", &target);
    let dir = libra_governor_daemon::paths::ensure_state_dir().unwrap();
    std::env::remove_var("LIBRA_GOVERNOR_STATE_DIR");

    assert_eq!(
        mode_of(&dir),
        0o700,
        "an existing, looser-permissioned state dir must be tightened, not left alone \
         — the security finding covers existing installs, not only fresh ones"
    );
}

#[test]
fn a_real_daemon_startup_hardens_the_socket_and_ledger_file_modes() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("daemon.sock");
    let ledger_path = dir.path().join("ledger.sqlite3");

    // Real `Permissions` before hardening: created with default
    // (umask-derived) mode by `UnixListener::bind` / `Connection::open`,
    // exactly the gate's finding.
    let listener = libra_governor_daemon::bind_or_detect_running(&socket_path).unwrap();
    let store = LedgerStore::open(&ledger_path).unwrap();
    drop(store);
    drop(listener);

    assert_eq!(
        mode_of(&socket_path),
        0o600,
        "the daemon's Unix socket must be owner-only (0600) after a real bind"
    );
    assert_eq!(
        mode_of(&ledger_path),
        0o600,
        "the SQLite ledger's main file must be owner-only (0600) after a real open"
    );

    // WAL-mode sidecar files: created lazily by SQLite, but if present
    // (they are, once WAL mode is on and anything has been written —
    // migrations apply inside a transaction at open time) must be
    // hardened too, not left at the default mode.
    for suffix in ["-wal", "-shm"] {
        let sidecar = dir.path().join(format!("ledger.sqlite3{suffix}"));
        if sidecar.exists() {
            assert_eq!(
                mode_of(&sidecar),
                0o600,
                "WAL sidecar {sidecar:?} must be owner-only (0600) too"
            );
        }
    }
}
