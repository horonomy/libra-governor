//! `libra-governor-ledger` — the local SQLite-backed store for task
//! trajectories: tasks, completion contracts, execution events, plans, and
//! receipts.
//!
//! # Concurrency model
//!
//! The store opens SQLite in WAL mode with a `busy_timeout`, so concurrent
//! readers never block a writer and a writer blocked on another writer
//! retries instead of failing immediately (see [`LedgerStore::open`]).
//! `LedgerStore` does not hold a long-lived connection open for the
//! caller: each [`LedgerStore`] wraps one `rusqlite::Connection`, and the
//! intended pattern for a multi-threaded or multi-process host (a hook
//! process and a daemon process, or several hook invocations) is one
//! `LedgerStore` per thread/process, each opening its own connection
//! against the same file. WAL mode plus `busy_timeout` makes that safe at
//! the single-machine scale MVP 1.0 targets; full multi-process admission
//! locking is out of scope here (see HORO-1141).
//!
//! # Privacy
//!
//! No table in this store's schema has a column for raw prompt text or
//! raw tool output. See `migrations/0001_init.sql` and
//! `libra-governor-domain`'s crate-level docs for the structural
//! guarantee this rests on.

mod migrations;
mod query;
mod store;
mod write;

pub use query::TaskTrajectory;
pub use store::LedgerStore;

/// Errors returned by the ledger crate.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("task {0} not found")]
    TaskNotFound(String),
}
