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
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LedgerError> {
        let mut conn = Connection::open(path)?;
        Self::configure(&mut conn)?;
        migrations::apply_all(&mut conn)?;
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

    fn configure(conn: &mut Connection) -> Result<(), LedgerError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000i64)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        Ok(())
    }
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
        assert_eq!(version, 7);
    }
}
