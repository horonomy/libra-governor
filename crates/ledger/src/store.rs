use std::path::Path;

use rusqlite::Connection;

use crate::{migrations, LedgerError};

/// A connection to the local trajectory store, with WAL mode and a busy
/// timeout applied so realistic concurrent single-machine usage (a hook
/// process and a daemon process both writing) does not corrupt or
/// double-count a task. See crate-level docs for the full concurrency
/// model.
pub struct LedgerStore {
    pub(crate) conn: Connection,
}

impl LedgerStore {
    /// Opens (creating if necessary) the SQLite database at `path`,
    /// applies any pending migrations, and returns a ready-to-use store.
    ///
    /// The main database file (and its `-wal`/`-shm` WAL-mode sidecar
    /// files, once [`Self::configure`] switches on WAL and they start
    /// existing) is hardened to owner-only (`0600`) after opening
    /// (HORO-1146 security review finding #5) — this ledger holds every
    /// task's contract, plan, and receipt evidence. This is defense in
    /// depth, not the primary boundary: the real protection is the
    /// containing state directory's own `0700` mode (see
    /// `libra-governor-daemon::paths::ensure_state_dir`), which is what
    /// actually protects a sidecar file the instant SQLite creates it,
    /// before this function's own `chmod` could run.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LedgerError> {
        let path = path.as_ref();
        let mut conn = Connection::open(path)?;
        Self::configure(&mut conn)?;
        migrations::apply_all(&mut conn)?;
        harden_file_permissions(path)?;
        Ok(Self { conn })
    }

    /// Opens an in-memory database (tests / ephemeral usage only — data
    /// does not survive process exit).
    pub fn open_in_memory() -> Result<Self, LedgerError> {
        let mut conn = Connection::open_in_memory()?;
        Self::configure(&mut conn)?;
        migrations::apply_all(&mut conn)?;
        Ok(Self { conn })
    }

    /// The highest `schema_migrations.version` actually applied to this
    /// open connection (HORO-1150's `doctor` diagnostic). Always present
    /// — `LedgerStore::open`/`open_in_memory` both run
    /// `migrations::apply_all` before returning, which creates the
    /// `schema_migrations` table unconditionally.
    pub fn schema_version(&self) -> Result<i64, LedgerError> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?)
    }

    fn configure(conn: &mut Connection) -> Result<(), LedgerError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000i64)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        Ok(())
    }
}

/// Sets the main ledger file, and its `-wal`/`-shm` WAL-mode sidecars (if
/// present — `open_in_memory` never has them, and even an on-disk store
/// only grows them once [`LedgerStore::configure`] has actually switched
/// the connection into WAL mode), to owner-only (`0600`) (HORO-1146).
/// Missing sidecars are not an error — SQLite creates them lazily on
/// first write, and a freshly opened, never-written-to store may not
/// have one yet.
fn harden_file_permissions(main_path: &Path) -> Result<(), LedgerError> {
    use std::os::unix::fs::PermissionsExt;

    let sidecar = |suffix: &str| {
        let mut name = main_path.file_name().unwrap_or_default().to_os_string();
        name.push(suffix);
        main_path.with_file_name(name)
    };

    for path in [main_path.to_path_buf(), sidecar("-wal"), sidecar("-shm")] {
        if path.exists() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_enables_wal_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite3");
        let store = LedgerStore::open(&path).unwrap();

        let mode: String = store
            .conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn reopening_same_file_preserves_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite3");
        {
            LedgerStore::open(&path).unwrap();
        }
        // Reopening an already-migrated file must not error or re-apply.
        let store = LedgerStore::open(&path).unwrap();
        let version: i64 = store
            .conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, 9);
    }

    #[test]
    fn schema_version_matches_the_latest_known_migration() {
        let store = LedgerStore::open_in_memory().unwrap();
        assert_eq!(
            store.schema_version().unwrap(),
            crate::migrations::latest_known_version()
        );
    }
}
