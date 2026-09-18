//! Embedded-SQL-file migration mechanism.
//!
//! Each migration is one `migrations/NNNN_description.sql` file, embedded
//! into the binary at compile time via [`include_str!`] and applied in
//! order inside a single transaction each. Applied versions are tracked in
//! a `schema_migrations` table so re-opening an already-migrated database
//! is a safe no-op — `apply_all` only ever applies versions strictly
//! greater than the current maximum recorded version.

use rusqlite::Connection;

use crate::LedgerError;

/// One embedded migration: a monotonically increasing version number and
/// its SQL body.
struct Migration {
    version: i64,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: include_str!("../migrations/0001_init.sql"),
    },
    Migration {
        version: 2,
        sql: include_str!("../migrations/0002_session_preflight_state.sql"),
    },
    Migration {
        version: 3,
        sql: include_str!("../migrations/0003_estimate_and_receipt_actuals.sql"),
    },
    Migration {
        version: 4,
        sql: include_str!("../migrations/0004_task_features.sql"),
    },
    Migration {
        version: 5,
        sql: include_str!("../migrations/0005_replanning.sql"),
    },
    Migration {
        version: 6,
        sql: include_str!("../migrations/0006_completion_reserve.sql"),
    },
];

/// Applies every migration whose version is greater than the database's
/// current recorded version, in ascending order. Safe to call on every
/// open — an already-migrated database is left untouched.
pub fn apply_all(conn: &mut Connection) -> Result<(), LedgerError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL
        );",
    )?;

    let current_version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;

    for migration in MIGRATIONS.iter().filter(|m| m.version > current_version) {
        let tx = conn.transaction()?;
        tx.execute_batch(migration.sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, datetime('now'))",
            [migration.version],
        )?;
        tx.commit()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applying_migrations_twice_is_a_no_op() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_all(&mut conn).unwrap();
        apply_all(&mut conn).unwrap();

        let version: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, MIGRATIONS.last().unwrap().version);

        let applied_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            applied_rows,
            MIGRATIONS.len() as i64,
            "each migration must be recorded exactly once"
        );
    }
}
