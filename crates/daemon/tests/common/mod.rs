//! Helpers shared by integration tests that need a raw view of the ledger file.

use std::path::Path;

/// Every table's rows as text, read through a separate read-only connection.
pub fn ledger_fingerprint(ledger: &Path) -> String {
    let conn =
        rusqlite::Connection::open_with_flags(ledger, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(Result::unwrap).collect()
    };
    let mut out = String::new();
    for table in tables {
        let mut stmt = conn.prepare(&format!("SELECT * FROM \"{table}\"")).unwrap();
        let columns = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        out.push_str(&format!("[{table}]\n"));
        while let Some(row) = rows.next().unwrap() {
            for i in 0..columns {
                out.push_str(&format!("{:?}|", row.get_ref(i).unwrap()));
            }
            out.push('\n');
        }
    }
    out
}
