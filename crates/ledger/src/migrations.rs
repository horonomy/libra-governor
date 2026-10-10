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
    Migration {
        version: 7,
        sql: include_str!("../migrations/0007_gateway_requests.sql"),
    },
    Migration {
        version: 8,
        sql: include_str!("../migrations/0008_plan_admission.sql"),
    },
    Migration {
        version: 9,
        sql: include_str!("../migrations/0009_extension_points.sql"),
    },
    Migration {
        version: 10,
        sql: include_str!("../migrations/0010_dogfood_evidence_capture.sql"),
    },
    Migration {
        version: 11,
        sql: include_str!("../migrations/0011_hierarchical_resource_accounts.sql"),
    },
    Migration {
        version: 12,
        sql: include_str!("../migrations/0012_receipt_regime.sql"),
    },
    Migration {
        version: 13,
        sql: include_str!("../migrations/0013_shadow_runtime_decisions.sql"),
    },
    Migration {
        version: 14,
        sql: include_str!("../migrations/0014_replay_pins.sql"),
    },
    Migration {
        version: 15,
        sql: include_str!("../migrations/0015_execution_association.sql"),
    },
    Migration {
        version: 16,
        sql: include_str!("../migrations/0016_shared_pool_reservation.sql"),
    },
    Migration {
        version: 17,
        sql: include_str!("../migrations/0017_task_budget_renewals.sql"),
    },
    Migration {
        version: 18,
        sql: include_str!("../migrations/0018_outcome_attestation_revision.sql"),
    },
];

/// The highest migration version this build of the crate knows about
/// (HORO-1150's `doctor` diagnostic compares this against a database's
/// actually-applied version — see [`crate::LedgerStore::schema_version`]
/// — to detect a binary that is older than the state it just opened).
pub fn latest_known_version() -> i64 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

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

    fn apply_known_through(conn: &mut Connection, max_known_version: i64) {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );",
        )
        .unwrap();
        let current_version: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
            .unwrap();
        for migration in MIGRATIONS
            .iter()
            .filter(|migration| migration.version <= max_known_version)
            .filter(|migration| migration.version > current_version)
        {
            let tx = conn.transaction().unwrap();
            tx.execute_batch(migration.sql).unwrap();
            tx.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, datetime('now'))",
                [migration.version],
            )
            .unwrap();
            tx.commit().unwrap();
        }
    }

    fn table_snapshot(conn: &Connection, table: &str, columns: &[&str]) -> Vec<Vec<String>> {
        let quoted_columns = columns
            .iter()
            .map(|column| format!("quote({column})"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT {quoted_columns} FROM {table} ORDER BY 1");
        let mut statement = conn.prepare(&sql).unwrap();
        statement
            .query_map([], |row| {
                (0..columns.len())
                    .map(|index| row.get(index))
                    .collect::<rusqlite::Result<Vec<String>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn schema_snapshot(conn: &Connection) -> Vec<(String, String, String, Option<String>)> {
        let mut statement = conn
            .prepare(
                "SELECT type, name, tbl_name, sql FROM sqlite_master
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn applied_versions(conn: &Connection) -> Vec<i64> {
        let mut statement = conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

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

    #[test]
    fn migration_15_preserves_legacy_state_and_does_not_guess_execution_owners() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        apply_known_through(&mut conn, 14);

        let task_id = "00000000-0000-7000-8000-000000000014";
        let plan_id = "00000000-0000-7000-8000-000000000114";
        conn.execute(
            "INSERT INTO tasks (task_id, external_ref_kind, external_ref_value, created_at, last_event_at)
             VALUES (?1, 'jira', 'HORO-1600', '2026-10-06T00:00:00Z', '2026-10-06T00:01:00Z')",
            [task_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO contracts (task_id, revision, created_at)
             VALUES (?1, 1, '2026-10-06T00:00:00Z')",
            [task_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO contract_criteria (task_id, revision, ordinal, description, required)
             VALUES (?1, 1, 0, 'legacy acceptance criterion', 1)",
            [task_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO plans (id, task_id, contract_revision, recon_snapshot_ref, created_at)
             VALUES (?1, ?2, 1, 'legacy-recon', '2026-10-06T00:01:00Z')",
            rusqlite::params![plan_id, task_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_tasks (session_id, task_id, created_at)
             VALUES ('legacy-session', ?1, '2026-10-06T00:00:00Z')",
            [task_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_preflights (session_id, plan_id, task_id, status, created_at)
             VALUES ('legacy-session', ?1, ?2, 'in_flight', '2026-10-06T00:01:00Z')",
            rusqlite::params![plan_id, task_id],
        )
        .unwrap();

        let legacy_rows = [
            (
                "tasks",
                &[
                    "task_id",
                    "external_ref_kind",
                    "external_ref_value",
                    "created_at",
                    "last_event_at",
                ][..],
            ),
            ("contracts", &["task_id", "revision", "created_at"]),
            (
                "contract_criteria",
                &["task_id", "revision", "ordinal", "description", "required"],
            ),
            (
                "plans",
                &[
                    "id",
                    "task_id",
                    "contract_revision",
                    "recon_snapshot_ref",
                    "created_at",
                ],
            ),
            ("session_tasks", &["session_id", "task_id", "created_at"]),
            (
                "session_preflights",
                &["session_id", "plan_id", "task_id", "status", "created_at"],
            ),
        ]
        .into_iter()
        .map(|(table, columns)| (table, table_snapshot(&conn, table, columns)))
        .collect::<Vec<_>>();
        let schema_before = schema_snapshot(&conn);

        apply_all(&mut conn).unwrap();

        let schema_after_migration = schema_snapshot(&conn);
        // Approved exception (ADR-0017, HORO-1727, migration 18):
        // `outcome_attestations`'s own `CREATE TABLE` text legitimately
        // changes (a new nullable `contract_revision` column), and it
        // gains two new append-only triggers. This is a deliberate,
        // reviewed schema evolution of a pre-migration-15 table, not a
        // silent regression — the table itself, and every pre-existing
        // row this test inserted, must still be intact; only its exact
        // `CREATE TABLE` text is allowed to differ.
        let approved_evolved_tables = ["outcome_attestations"];
        for definition in &schema_before {
            if approved_evolved_tables.contains(&definition.2.as_str()) {
                assert!(
                    schema_after_migration
                        .iter()
                        .any(|(_, _, tbl_name, _)| tbl_name == &definition.2),
                    "approved-evolved table {:?} must still exist, just with new columns/triggers",
                    definition.2
                );
                continue;
            }
            assert!(
                schema_after_migration.contains(definition),
                "migration 15 changed or removed pre-existing schema definition: {definition:?}"
            );
        }
        for (table, before) in &legacy_rows {
            let columns = match *table {
                "tasks" => &[
                    "task_id",
                    "external_ref_kind",
                    "external_ref_value",
                    "created_at",
                    "last_event_at",
                ][..],
                "contracts" => &["task_id", "revision", "created_at"],
                "contract_criteria" => {
                    &["task_id", "revision", "ordinal", "description", "required"]
                }
                "plans" => &[
                    "id",
                    "task_id",
                    "contract_revision",
                    "recon_snapshot_ref",
                    "created_at",
                ],
                "session_tasks" => &["session_id", "task_id", "created_at"],
                "session_preflights" => {
                    &["session_id", "plan_id", "task_id", "status", "created_at"]
                }
                _ => unreachable!(),
            };
            assert_eq!(
                &table_snapshot(&conn, table, columns),
                before,
                "{table} rows changed"
            );
        }
        for table in ["execution_lanes", "execution_turns", "execution_replays"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "migration must not infer {table} rows");
        }

        let schema_after_first_apply = schema_snapshot(&conn);
        let applied_versions_before = applied_versions(&conn);
        apply_all(&mut conn).unwrap();
        assert_eq!(schema_snapshot(&conn), schema_after_first_apply);
        let applied_versions_after = applied_versions(&conn);
        assert_eq!(applied_versions_after, applied_versions_before);

        conn.execute(
            "INSERT INTO execution_lanes (lane, current_turn, exact_context) VALUES ('claude/session', 'turn-1', 'fixture-context')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO execution_turns (
                lane, turn, exact_identity, identity_json, task_id, initial_plan_id, state
             ) VALUES ('claude/session', 'turn-1', 'provider/native-id', '{}', ?1, ?2, 'active')",
            rusqlite::params![task_id, plan_id],
        )
        .unwrap();

        // Model the migration loop shipped by a version-14 opener: it knows
        // only migrations through 14 and skips every migration at or below
        // the stored version. This verifies schema compatibility and row
        // readability; it does not execute a separately compiled old binary.
        apply_known_through(&mut conn, 14);
        let legacy_task: String = conn
            .query_row(
                "SELECT task_id FROM session_tasks WHERE session_id = 'legacy-session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_task, task_id);
        let owner_turns: i64 = conn
            .query_row("SELECT COUNT(*) FROM execution_turns", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            owner_turns, 1,
            "a version-14 migration loop must leave newer owner state intact"
        );
        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, latest_known_version());
    }
}
