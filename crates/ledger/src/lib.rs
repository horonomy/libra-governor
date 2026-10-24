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

mod economic_truth;
mod execution_owner;
pub use execution_owner::ExecutionLookup;
mod extension;
mod gateway;
mod migrations;
mod query;
mod renewal;
mod reservation;
mod resource_account;
mod session;
mod shared_pool;
mod shared_pool_retention;
mod store;
mod transaction;
mod write;

pub use extension::{
    BusinessContextInsert, DeliveryEnqueue, OutcomeAttestationInsert, QueuedDeliveryRow,
    RecordAttestationOutcome,
};
pub use gateway::{GatewayRequestClose, GatewayRequestOpen};
pub use migrations::latest_known_version;

/// Denomination marker stored in `receipts.usage_basis` for usage measured
/// as deduplicated fresh tokens (input + cache_creation + output, no
/// cache_read) -- see migration `0019`. Receipts without it are not
/// comparable and are withheld from resource calibration.
pub const USAGE_BASIS_FRESH_DEDUP_V1: &str = "fresh_dedup_v1";
pub use query::{
    CalibrationPair, DogfoodPlanRecord, DogfoodReceiptRecord, EvidenceAggregates, TaskTrajectory,
};
pub use renewal::GrantRenewalOutcome;
pub use reservation::{
    AdjustOutcome, ReleaseOutcome, ReserveOutcome, ReserveRequest, SettleOutcome,
};
pub use resource_account::{
    AccountError, EnsureAccountOutcome, GrantSubleaseOutcome, GrantSubleaseRequest,
    RecordedShadowDecision,
};
pub use shared_pool::{
    CapacitySource, ExecutorAttribution, MeteredReserveOutcome, MeteredReserveRequest,
    SharedPoolChargeOutcome, SharedPoolChargeRequest, SharedPoolExecutorReport,
    SharedPoolExecutorUsage, SharedPoolReleaseOutcome, SharedPoolRenewOutcome,
    SharedPoolReserveOutcome, SharedPoolReserveRequest, SharedPoolSessionUsage,
    SharedPoolSettleOutcome, SourcedAdmission,
};
pub use shared_pool_retention::{
    ChargeSourceTotals, PoolExport, PoolExportExecutor, PoolPruneDetail, PruneDisposition,
    PruneReport, PruneRequest, RetentionError,
};
pub use store::{LedgerStore, APPEND_ONLY_TRIGGERS};
pub use write::DOGFOOD_PROFILE_ENV_VAR;

/// Errors returned by the ledger crate.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("transaction state unconfirmed; retry durable replay lookup")]
    TransactionUnconfirmed,
    #[error("inconsistent execution association")]
    InvalidExecutionAssociation,
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A filesystem operation on the ledger's own files failed — as of
    /// HORO-1146, only `LedgerStore::open`'s post-open `chmod` to
    /// owner-only permissions (see `store::harden_file_permissions`)
    /// raises this.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
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
    /// A shared-pool reserve/ingest amount's
    /// [`libra_governor_domain::QuotaUnit`] does not match the pool's own
    /// declared unit (HORO-1763) — the shared-pool analogue of
    /// [`Self::ResourceKindMismatch`].
    #[error("quota unit {actual:?} does not match pool unit {expected:?}")]
    QuotaUnitMismatch {
        expected: libra_governor_domain::QuotaUnit,
        actual: libra_governor_domain::QuotaUnit,
    },
    /// An ingest targeted a pool id with no `quota_pools` row
    /// (HORO-1915) — `ingest_pool_provider_snapshot` never implicitly
    /// creates one; see that method's own docs.
    #[error("no quota pool exists for pool id {pool_id:?}")]
    NoSuchPool { pool_id: String },
    /// A reserve named an idempotency key that already belongs to a
    /// `charge_shared` row (HORO-2194): that is recorded spend, not a hold,
    /// so it is refused rather than replayed as `AlreadyGranted`.
    #[error("idempotency key belongs to a recorded charge, not a hold")]
    SharedPoolKeyConflict,
}
