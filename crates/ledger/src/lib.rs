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
mod reservation;
mod session;
mod store;
mod write;

pub use query::{CalibrationPair, TaskTrajectory};
pub use reservation::{
    AdjustOutcome, ReleaseOutcome, ReserveOutcome, ReserveRequest, SettleOutcome,
};
pub use store::LedgerStore;

/// Errors returned by the ledger crate.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("task {0} not found")]
    TaskNotFound(String),
    /// A reservation/settlement amount's [`libra_governor_domain::ResourceKind`]
    /// does not match the task's own `task_budgets.resource_kind`
    /// (HORO-1141) — part of the "malicious/invalid agent event cannot
    /// directly forge ledger spend/credit" failure case: a mismatched
    /// kind is rejected outright rather than silently coerced.
    #[error("resource kind {actual:?} does not match task budget kind {expected:?}")]
    ResourceKindMismatch {
        expected: libra_governor_domain::ResourceKind,
        actual: libra_governor_domain::ResourceKind,
    },
    /// A settlement reported a negative actual-usage amount — never a
    /// legitimate value, and rejected rather than silently treated as a
    /// credit (HORO-1141 "malicious/invalid agent event" failure case).
    #[error("a negative resource amount is not a valid settlement")]
    NegativeSettlement,
}
